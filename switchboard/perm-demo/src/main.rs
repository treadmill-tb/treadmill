#![allow(dead_code)]

use std::fmt::Debug;
use std::marker::PhantomData;

pub trait PrivilegedAction: Debug
where
    Self: Sized,
{
    fn authorize(self, auth_token: &str) -> Result<Privilege<Self>, IssuanceError>;
}

#[derive(Debug, Clone)]
pub struct User(usize);
#[derive(Debug, Clone)]
pub struct Group(usize);
#[derive(Debug, Clone)]
pub struct Token(usize);

#[derive(Debug, Clone)]
pub enum Subject {
    User(User),
    Token(Token),
}

#[derive(Debug)]
pub struct Privilege<'a, Action: PrivilegedAction> {
    subject: Subject,
    path: Vec<Group>,
    action: Action,
    _pd: PhantomData<&'a ()>,
}

impl<Action: PrivilegedAction> Privilege<'_, Action> {
    pub fn subject(&self) -> &Subject {
        &self.subject
    }

    pub fn path(&self) -> &[Group] {
        &self.path
    }

    pub fn action(&self) -> &Action {
        &self.action
    }
}

#[derive(Debug)]
pub enum IssuanceError {
    Unauthorized,
}

pub trait PrivilegeSource {
    fn issue<'a, A: PrivilegedAction>(
        &'a self,
        action: A,
    ) -> Result<Privilege<'a, A>, IssuanceError>; // {
}

#[derive(Debug, Clone)]
pub struct MockPrivilegeSource;

#[derive(Debug, Clone)]
pub struct MockPrivilegeSourceGuard {
    source: MockPrivilegeSource,
    auth_token: String,
}

impl PrivilegeSource for MockPrivilegeSourceGuard {
    fn issue<'a, A: PrivilegedAction>(
        &'a self,
        action: A,
    ) -> Result<Privilege<'a, A>, IssuanceError> {
        action.authorize(&self.auth_token)
    }
}

#[derive(Debug, Clone)]
pub struct EnqueueCIJobAction {
    pub supervisor_id: String,
}

impl PrivilegedAction for EnqueueCIJobAction {
    fn authorize(self, priv_exec: PrivilegeQueryExec) -> Result<Privilege<Self>, IssuanceError> {
        priv_exec
            .query(format!("enqueue_ci_job:{}", &self.supervisor_id))
            .try_into_privilege(self)
    }
}

fn enqueue_ci_job(p: Privilege<EnqueueCIJobAction>) {
    // do things
    let subject = p.subject();
    let object = &p.action().supervisor_id;
    println!("[subject:{subject:?}] enqueuing a job on [object:{object:?}]");
}

fn main() {
    //tracing_subscriber::fmt()
    //    .with_env_filter(EnvFilter::from_default_env())
    //    .init();

    let mps = MockPrivilegeSourceGuard {
        source: MockPrivilegeSource,
        auth_token: "baz".to_string(),
    };

    let privilege = mps
        .issue(EnqueueCIJobAction {
            supervisor_id: "foobar".to_string(),
        })
        .unwrap();

    enqueue_ci_job(privilege);
}
