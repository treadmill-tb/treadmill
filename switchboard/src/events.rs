//! In-process fan-out for the `tml_events` LISTEN/NOTIFY channel (see the
//! CHANGE NOTIFICATIONS section of SCHEMA.sql for the emitting triggers and
//! the payload contract).
//!
//! Notifications are edge-triggered wake-ups, not data carriers: a consumer's
//! reaction to being woken is to re-read the database, exactly as on its
//! periodic timer, which stays in place as the staleness fallback. This makes
//! lost notifications (listener reconnect gaps) and duplicated or reordered
//! ones harmless, and reduces rate limiting to coalescing plus a per-consumer
//! debounce.
//!
//! One [`EventBus`] (and one Postgres `LISTEN` connection, via
//! [`EventBus::listener`]) exists per process; each `LISTEN` pins a dedicated
//! database connection, so per-consumer listeners would exhaust the pool. The
//! subscription registry is ephemeral in-process routing state only — it is
//! rebuilt from nothing on restart and correctness never depends on it.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::watch;
use tokio::time::Instant;
use uuid::Uuid;

const CHANNEL: &str = "tml_events";
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

#[derive(serde::Deserialize)]
struct Event {
    table: String,
    /// Routing-key values, each rendered as its jsonb scalar. Absent/empty on
    /// the bulk-write overflow form, which wakes every subscriber of `table`.
    #[serde(default)]
    keys: HashMap<String, Vec<serde_json::Value>>,
}

/// What a subscription matches: one table, either any row (`key: None`) or
/// rows whose routing key `column` carries `value`. The emitting trigger
/// decides which columns are routing keys; the primary key always is.
///
/// Clients must expect spurious notifications and ones that don't
/// include even the primary, for instance in the case of bulk
/// updates / inserts.
pub struct EventFilter {
    pub table: &'static str,
    pub key: Option<(&'static str, Uuid)>,
}

#[derive(Clone, Default)]
pub struct EventBus {
    registry: Arc<Mutex<Registry>>,
}

#[derive(Default)]
struct Registry {
    tables: HashMap<String, TableSubs>,
}

#[derive(Default)]
struct TableSubs {
    wildcard: Vec<watch::Sender<()>>,
    /// column -> value -> subscribers.
    keyed: HashMap<String, HashMap<String, Vec<watch::Sender<()>>>>,
}

fn wake(tx: &watch::Sender<()>) -> bool {
    tx.send(()).is_ok()
}

impl EventBus {
    /// Register a subscription. It starts woken, so there is no gap between
    /// subscribing and the consumer's first pass over current DB state.
    pub fn subscribe(&self, filter: EventFilter) -> WakeSubscription {
        let (tx, mut rx) = watch::channel(());
        rx.mark_changed();
        let mut registry = self.registry.lock().unwrap();
        let table = registry.tables.entry(filter.table.to_string()).or_default();
        match filter.key {
            None => table.wildcard.push(tx),
            Some((column, value)) => table
                .keyed
                .entry(column.to_string())
                .or_default()
                .entry(value.to_string())
                .or_default()
                .push(tx),
        }
        WakeSubscription { rx }
    }

    /// The long-running listener feeding this bus; spawn exactly one per
    /// process. Reconnects forever; every (re)connect wakes all subscriptions,
    /// since notifications during the gap are silently lost.
    pub fn listener(&self, pool: PgPool) -> impl Future<Output = ()> + Send + 'static {
        let bus = self.clone();
        async move {
            loop {
                match PgListener::connect_with(&pool).await {
                    Ok(mut listener) => match listener.listen(CHANNEL).await {
                        Ok(()) => {
                            bus.wake_all();
                            bus.pump(&mut listener).await;
                        }
                        Err(e) => tracing::warn!("event listener LISTEN failed: {e}"),
                    },
                    Err(e) => tracing::warn!("event listener connect failed: {e}"),
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }

    async fn pump(&self, listener: &mut PgListener) {
        loop {
            match listener.try_recv().await {
                Ok(Some(notification)) => self.handle_payload(notification.payload()),
                // The connection was lost and reestablished: a gap.
                Ok(None) => self.wake_all(),
                Err(e) => {
                    tracing::warn!("event listener receive failed: {e}");
                    return;
                }
            }
        }
    }

    fn handle_payload(&self, payload: &str) {
        let event: Event = match serde_json::from_str(payload) {
            Ok(event) => event,
            Err(e) => {
                tracing::warn!("malformed tml_events payload: {e}");
                return;
            }
        };
        let mut registry = self.registry.lock().unwrap();
        let Some(table) = registry.tables.get_mut(&event.table) else {
            return;
        };
        table.wildcard.retain(wake);
        if event.keys.is_empty() {
            for by_value in table.keyed.values_mut() {
                for subs in by_value.values_mut() {
                    subs.retain(wake);
                }
            }
        } else {
            for (column, values) in &event.keys {
                let Some(by_value) = table.keyed.get_mut(column) else {
                    continue;
                };
                for value in values {
                    let value = match value.as_str() {
                        Some(s) => s.to_string(),
                        None => value.to_string(),
                    };
                    if let Some(subs) = by_value.get_mut(&value) {
                        subs.retain(wake);
                    }
                }
            }
        }
    }

    fn wake_all(&self) {
        let mut registry = self.registry.lock().unwrap();
        for table in registry.tables.values_mut() {
            table.wildcard.retain(wake);
            for by_value in table.keyed.values_mut() {
                for subs in by_value.values_mut() {
                    subs.retain(wake);
                }
            }
        }
    }
}

/// A coalescing wake handle: any number of matching events while the consumer
/// is busy collapse into one pending wake.
pub struct WakeSubscription {
    rx: watch::Receiver<()>,
}

impl WakeSubscription {
    /// Wait until at least one matching event arrived since the last call.
    /// Never resolves if the bus is gone (the consumer's timer still drives
    /// it).
    pub async fn changed(&mut self) {
        if self.rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Rate limiter over one or more subscriptions: leading-edge (an isolated
/// event fires immediately), a minimum `cooldown` between firings under
/// sustained events, and a trailing firing for events that arrived during the
/// cooldown.
pub struct Debounced {
    subs: Vec<WakeSubscription>,
    cooldown: Duration,
    last_fired: Option<Instant>,
}

impl Debounced {
    pub fn new(subs: Vec<WakeSubscription>, cooldown: Duration) -> Self {
        assert!(!subs.is_empty());
        Self {
            subs,
            cooldown,
            last_fired: None,
        }
    }

    /// Cancellation-safe (fit for a `select!` arm): the cooldown is waited out
    /// *before* any wake is consumed, and consumption happens synchronously in
    /// the resolving poll — a `wait` future dropped mid-flight never loses a
    /// pending wake.
    pub async fn wait(&mut self) {
        if let Some(last_fired) = self.last_fired {
            tokio::time::sleep_until(last_fired + self.cooldown).await;
        }
        {
            let changed: Vec<_> = self
                .subs
                .iter_mut()
                .map(|sub| Box::pin(sub.changed()))
                .collect();
            futures_util::future::select_all(changed).await;
        }
        // Fold everything that accumulated while waiting into this firing;
        // the caller's imminent pass reads fresh state and covers it all.
        for sub in &mut self.subs {
            let _ = sub.changed().now_or_never();
        }
        self.last_fired = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn woken(sub: &mut WakeSubscription) -> bool {
        sub.changed().now_or_never().is_some()
    }

    fn wildcard(bus: &EventBus, table: &'static str) -> WakeSubscription {
        bus.subscribe(EventFilter { table, key: None })
    }

    fn keyed(
        bus: &EventBus,
        table: &'static str,
        column: &'static str,
        value: Uuid,
    ) -> WakeSubscription {
        bus.subscribe(EventFilter {
            table,
            key: Some((column, value)),
        })
    }

    fn drain(subs: &mut [&mut WakeSubscription]) {
        for sub in subs {
            assert!(woken(sub), "a fresh subscription must start woken");
        }
    }

    #[tokio::test]
    async fn subscription_starts_woken_then_coalesces() {
        let bus = EventBus::default();
        let mut sub = wildcard(&bus, "jobs");
        assert!(woken(&mut sub));
        assert!(!woken(&mut sub));

        bus.handle_payload(r#"{"table":"jobs","keys":{"job_id":["x"]}}"#);
        bus.handle_payload(r#"{"table":"jobs","keys":{"job_id":["y"]}}"#);
        assert!(woken(&mut sub), "events wake the subscription");
        assert!(!woken(&mut sub), "two events coalesce into one wake");
    }

    #[tokio::test]
    async fn keyed_and_wildcard_filtering() {
        let bus = EventBus::default();
        let hit = Uuid::new_v4();
        let miss = Uuid::new_v4();
        let mut wild = wildcard(&bus, "jobs");
        let mut keyed_hit = keyed(&bus, "jobs", "job_id", hit);
        let mut keyed_miss = keyed(&bus, "jobs", "job_id", miss);
        let mut other_column = keyed(&bus, "jobs", "dispatched_on_host_id", hit);
        let mut other_table = wildcard(&bus, "hosts");
        drain(&mut [
            &mut wild,
            &mut keyed_hit,
            &mut keyed_miss,
            &mut other_column,
            &mut other_table,
        ]);

        bus.handle_payload(&format!(
            r#"{{"table":"jobs","keys":{{"job_id":["{hit}"]}}}}"#
        ));
        assert!(woken(&mut wild));
        assert!(woken(&mut keyed_hit));
        assert!(!woken(&mut keyed_miss));
        assert!(!woken(&mut other_column));
        assert!(!woken(&mut other_table));
    }

    #[tokio::test]
    async fn multi_value_key_wakes_both_scopes() {
        let bus = EventBus::default();
        let old = Uuid::new_v4();
        let new = Uuid::new_v4();
        let mut sub_old = keyed(&bus, "jobs", "dispatched_on_host_id", old);
        let mut sub_new = keyed(&bus, "jobs", "dispatched_on_host_id", new);
        drain(&mut [&mut sub_old, &mut sub_new]);

        bus.handle_payload(&format!(
            r#"{{"table":"jobs","keys":{{"job_id":["j"],"dispatched_on_host_id":["{old}","{new}"]}}}}"#
        ));
        assert!(woken(&mut sub_old));
        assert!(woken(&mut sub_new));
    }

    #[tokio::test]
    async fn keyless_event_wakes_every_table_subscriber() {
        let bus = EventBus::default();
        let mut wild = wildcard(&bus, "hosts");
        let mut keyed_sub = keyed(&bus, "hosts", "host_id", Uuid::new_v4());
        let mut other_table = wildcard(&bus, "jobs");
        drain(&mut [&mut wild, &mut keyed_sub, &mut other_table]);

        bus.handle_payload(r#"{"table":"hosts"}"#);
        assert!(woken(&mut wild));
        assert!(woken(&mut keyed_sub));
        assert!(!woken(&mut other_table));
    }

    #[tokio::test]
    async fn wake_all_wakes_everything() {
        let bus = EventBus::default();
        let mut wild = wildcard(&bus, "jobs");
        let mut keyed_sub = keyed(&bus, "hosts", "host_id", Uuid::new_v4());
        drain(&mut [&mut wild, &mut keyed_sub]);

        bus.wake_all();
        assert!(woken(&mut wild));
        assert!(woken(&mut keyed_sub));
    }

    #[tokio::test]
    async fn malformed_payload_wakes_nothing() {
        let bus = EventBus::default();
        let mut sub = wildcard(&bus, "jobs");
        drain(&mut [&mut sub]);

        bus.handle_payload("not json");
        bus.handle_payload(r#"{"keys":{}}"#);
        assert!(!woken(&mut sub));
    }

    #[tokio::test]
    async fn dropped_subscriptions_are_pruned() {
        let bus = EventBus::default();
        let sub = wildcard(&bus, "jobs");
        drop(sub);
        bus.handle_payload(r#"{"table":"jobs","keys":{"job_id":["x"]}}"#);
        let registry = bus.registry.lock().unwrap();
        assert!(registry.tables["jobs"].wildcard.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn debounce_is_leading_edge_with_cooldown() {
        let cooldown = Duration::from_secs(1);
        let bus = EventBus::default();
        let sub = wildcard(&bus, "jobs");
        let mut debounced = Debounced::new(vec![sub], cooldown);

        // Leading edge: the initial (start-woken) firing is immediate.
        let start = Instant::now();
        debounced.wait().await;
        assert_eq!(Instant::now() - start, Duration::ZERO);

        // An event right after a firing is held back to the cooldown.
        bus.handle_payload(r#"{"table":"jobs","keys":{"job_id":["x"]}}"#);
        debounced.wait().await;
        assert_eq!(Instant::now() - start, cooldown);

        // After a quiet stretch longer than the cooldown, leading edge again.
        tokio::time::sleep(cooldown * 5).await;
        bus.handle_payload(r#"{"table":"jobs","keys":{"job_id":["y"]}}"#);
        let quiet = Instant::now();
        debounced.wait().await;
        assert_eq!(Instant::now() - quiet, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn debounce_folds_accumulated_wakes_into_one_firing() {
        let bus = EventBus::default();
        let sub_a = wildcard(&bus, "jobs");
        let sub_b = wildcard(&bus, "hosts");
        let mut debounced = Debounced::new(vec![sub_a, sub_b], Duration::from_secs(1));

        bus.handle_payload(r#"{"table":"jobs","keys":{"job_id":["x"]}}"#);
        bus.handle_payload(r#"{"table":"hosts","keys":{"host_id":["y"]}}"#);
        debounced.wait().await;

        // Both wakes (and the start-woken state) folded into the firing above:
        // nothing further is pending.
        assert!(debounced.wait().now_or_never().is_none());
    }
}
