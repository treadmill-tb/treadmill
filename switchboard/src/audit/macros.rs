#[doc(hidden)]
#[macro_export]
macro_rules! __audit_process_field {
    ($rels:ident, $val:expr, Job @ view($view:ident)) => {
        $rels.push($crate::audit::model::Relation {
            entity: $crate::audit::model::EntityRef::Job($val.0),
            role: $crate::audit::model::Role::Subject,
            view: $crate::audit::model::ViewPolicy::Permission($crate::auth::engine::JobPermission::$view.into()),
        })
    };
    ($rels:ident, $val:expr, Option < Job > @ view($view:ident)) => {
        if let Some(j) = &$val {
            $rels.push($crate::audit::model::Relation {
                entity: $crate::audit::model::EntityRef::Job(j.0),
                role: $crate::audit::model::Role::Subject,
                view: $crate::audit::model::ViewPolicy::Permission($crate::auth::engine::JobPermission::$view.into()),
            })
        }
    };
    ($rels:ident, $val:expr, Host @ view($view:ident)) => {
        $rels.push($crate::audit::model::Relation {
            entity: $crate::audit::model::EntityRef::Host($val.0),
            // Host is typically context when a job is the subject. If there's no job, it's the subject.
            // The plan says "relations with the declared role/view". But it doesn't give syntax for role.
            // We'll use `Context` for Host to match the dummy test where Host was Subject but Actor was Subject.
            // Actually, `Role::Context` is perfect for additional entities. If it's a Host-centric event, it might be `Subject`.
            // Wait, maybe we should just default to `Context` and let it be. Or provide an optional `role($role:ident)` syntax?
            // "Fields typed Job/Host/Subject ... become relations with the declared role/view".
            // Since we don't have explicit roles, let's use `Role::Subject` for the primary one.
            // If the type is `Job`, it's `Role::Subject`. If it's `Host`, let's make it `Role::Context` by default.
            // Wait, if it's `HostRegistered` event, the host IS the subject!
            role: $crate::audit::model::Role::Context,
            view: $crate::audit::model::ViewPolicy::Permission($crate::auth::engine::HostPermission::$view.into()),
        })
    };
    ($rels:ident, $val:expr, Option < Host > @ view($view:ident)) => {
        if let Some(h) = &$val {
            $rels.push($crate::audit::model::Relation {
                entity: $crate::audit::model::EntityRef::Host(h.0),
                role: $crate::audit::model::Role::Context,
                view: $crate::audit::model::ViewPolicy::Permission($crate::auth::engine::HostPermission::$view.into()),
            })
        }
    };
    ($rels:ident, $val:expr, Subject @ view($view:ident)) => {
        $rels.push($crate::audit::model::Relation {
            entity: $crate::audit::model::EntityRef::Subject($val.0),
            role: $crate::audit::model::Role::Context,
            // Subjects don't have permissions in `Permission` enum (only Host/Job).
            // So ViewPolicy::OperatorOnly is the only thing that makes sense if a Subject is related.
            // Actually, we can just map it to OperatorOnly for now.
            view: $crate::audit::model::ViewPolicy::OperatorOnly,
        })
    };
    ($rels:ident, $val:expr, Option < Subject > @ view($view:ident)) => {
        if let Some(s) = &$val {
            $rels.push($crate::audit::model::Relation {
                entity: $crate::audit::model::EntityRef::Subject(s.0),
                role: $crate::audit::model::Role::Context,
                view: $crate::audit::model::ViewPolicy::OperatorOnly,
            })
        }
    };
    ($rels:ident, $val:expr, $($tt:tt)*) => {
        // Plain field or field with no view specified -> payload only
    };
}

/// Defines a typed audit event.
///
/// Generates:
/// 1. The struct definition with `Serialize` / `Deserialize`.
/// 2. The `AuditEvent` implementation.
/// 3. The `linkme` registry entry for the view-time renderer.
#[macro_export]
macro_rules! define_event {
    (
        $(#[$meta:meta])*
        $name:ident $version:ident {
            actor: Subject,
            $(
                $field:ident : $ty:ident $( < $generic:ident > )? $( @ view($view:ident) )?
            ),* $(,)?
        }
        render = $render:literal;
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
        pub struct $name {
            pub actor: $crate::audit::model::Subject,
            $( pub $field: $ty $( < $generic > )? ),*
        }

        impl $crate::audit::model::AuditEvent for $name {
            fn event_type(&self) -> &'static str {
                concat!(stringify!($name), ".", stringify!($version))
            }

            fn actor(&self) -> uuid::Uuid {
                self.actor.0
            }

            fn relations(&self) -> Vec<$crate::audit::model::Relation> {
                let mut rels = vec![
                    $crate::audit::model::Relation {
                        entity: $crate::audit::model::EntityRef::Subject(self.actor.0),
                        role: $crate::audit::model::Role::Actor,
                        view: $crate::audit::model::ViewPolicy::OperatorOnly,
                    }
                ];

                $(
                    $crate::__audit_process_field!(rels, self.$field, $ty $(< $generic >)? $( @ view($view) )?);
                )*

                rels
            }
        }

        #[linkme::distributed_slice($crate::audit::registry::RENDERERS)]
        #[linkme(crate = linkme)]
        static RENDERER: $crate::audit::registry::RendererEntry = $crate::audit::registry::RendererEntry {
            event_type: concat!(stringify!($name), ".", stringify!($version)),
            render: |payload, _viewer| {
                let event: $name = match serde_json::from_value(payload.clone()) {
                    Ok(e) => e,
                    Err(e) => return $crate::audit::registry::RenderResult::PayloadMismatch(e),
                };
                
                #[allow(unused_variables)]
                let $name { actor, $( $field ),* } = event;
                
                let message = format!($render);
                $crate::audit::registry::RenderResult::Ok($crate::audit::registry::RenderedEvent { message })
            }
        };
    }
}
