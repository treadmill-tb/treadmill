// This is scaffolding for an "editor" that allows traversing an
// object tree and setting + deleting configuration attributes. It's
// currently not in use.

use crate::cli;

pub trait EditTreeNode {
    fn children(&self) -> Vec<String>;
    fn show(&self);
    fn get(&self, key: &str) -> &dyn EditTreeNode;
}

#[derive(Clone, Debug)]
pub enum EditTreeCommand {
    Show,
    Up,
    Top,
    Set(Vec<String>),
    Get(Vec<String>),
    Edit(Vec<String>),
}

#[derive(Default, Clone, Debug)]
pub struct EditTree {
    current_path: Vec<String>,
}

impl EditTree {
    pub fn reset_context(&mut self) {
        self.current_path = vec![];
    }

    pub fn parse<'a>(
        &self,
        input: cli::ParseInput<'a, Self>,
    ) -> cli::ParseChain<'a, Self, EditTreeCommand> {
        input.parse().missing_unknown_command_error()
    }

    pub fn execute(&mut self, _parsed: &EditTreeCommand) {
        unimplemented!()
    }
}
