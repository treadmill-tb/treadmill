#![allow(dead_code)]

use std::fmt::Debug;
use tracing_subscriber::EnvFilter;

pub trait PrivilegedAction: Debug
where
    Self: Sized,
{
    const ACTION: &'static str;
    type ObjectTy: Object;

    fn new() -> Self;
}

#[derive(Debug, Clone)]
pub struct User(usize);
#[derive(Debug, Clone)]
pub struct Group(usize);
#[derive(Debug, Clone)]
pub struct Token(usize);
#[derive(Debug, Clone)]
pub struct Overseer(usize);
#[derive(Debug, Clone)]
pub enum Subject {
    User(User),
    Group(Group),
    Token(Token),
}
trait ObjectFragment: Clone + Debug {}
pub trait Object: Clone + Debug {}
impl ObjectFragment for User {}
impl ObjectFragment for Group {}
impl ObjectFragment for Token {}
impl ObjectFragment for Overseer {}

impl<O1: ObjectFragment> Object for O1 {}
impl<O1: ObjectFragment> Object for (O1,) {}
impl<O1: ObjectFragment, O2: ObjectFragment> Object for (O1, O2) {}
impl<O1: ObjectFragment, O2: ObjectFragment, O3: ObjectFragment> Object for (O1, O2, O3) {}

#[derive(Debug)]
pub struct Privilege<Action: PrivilegedAction> {
    subject: Subject,
    action: Action,
    object: Action::ObjectTy,
}
impl<Action: PrivilegedAction> Privilege<Action> {
    pub fn extract(self) -> (Subject, Action, Action::ObjectTy) {
        let Self {
            subject,
            action,
            object,
        } = self;
        (subject, action, object)
    }
}

#[derive(Debug)]
pub enum IssuanceError {
    Unauthorized,
}

pub trait PrivilegeSource {
    fn issue<Action: PrivilegedAction>(
        &self,
        subject: &Subject,
        object: &Action::ObjectTy,
    ) -> Result<Privilege<Action>, IssuanceError> {
        if self.can_issue(subject, Action::ACTION, object) {
            Ok(Privilege {
                subject: subject.clone(),
                action: Action::new(),
                object: object.clone(),
            })
        } else {
            Err(IssuanceError::Unauthorized)
        }
    }
    fn can_issue<O: Object>(&self, subject: &Subject, action: &str, object: &O) -> bool;
}

#[derive(Debug, Copy, Clone)]
pub struct MockPrivilegeSource {}
impl PrivilegeSource for MockPrivilegeSource {
    fn can_issue<O: Object>(&self, subject: &Subject, action: &str, object: &O) -> bool {
        // internal logic goes here
        tracing::debug!("can-issue: can we [subject:{subject:?}] perform action [action:{action:?}] on object [object:{object:?}]?");
        tracing::debug!("(>^_^)> MockPrivilegeSource is automatically saying yes <(^_^<)");
        true
    }
}

#[derive(Debug, Copy, Clone)]
pub struct EnqueueCIJobPrivilege;
impl PrivilegedAction for EnqueueCIJobPrivilege {
    const ACTION: &'static str = "api.enqueue_ci_job:";
    type ObjectTy = Overseer;

    fn new() -> Self {
        Self
    }
}

fn enqueue_ci_job(privilege: Privilege<EnqueueCIJobPrivilege>) {
    let (subject, _, object) = privilege.extract();
    // do things
    tracing::info!("[subject:{subject:?}] enqueuing a job on [object:{object:?}]");
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let mps = MockPrivilegeSource {};

    let my_group = Group(0);
    let overseer = Overseer(1);

    //
    let privilege = mps.issue::<EnqueueCIJobPrivilege>(&Subject::Group(my_group), &overseer);
    enqueue_ci_job(privilege.unwrap());
}
