//! Recipes by intent and executors by action name: the only list of what the owner can ask for and
//! what can act outside gm.

use std::collections::HashMap;
use std::sync::Arc;

use super::{Executor, Intent, Recipe};

#[derive(Clone, Default)]
pub struct Registry {
    recipes: HashMap<Intent, Arc<dyn Recipe>>,
    executors: HashMap<&'static str, Arc<dyn Executor>>,
}

impl Registry {
    /// Translate, analyze and reply ([`crate::recipes`]) and the `gmail.send_draft` executor.
    pub fn standard() -> Self {
        let mut r = Self::default();
        r.add_recipe(Arc::new(crate::recipes::translate::Translate));
        r.add_recipe(Arc::new(crate::recipes::analyze::Analyze));
        r.add_recipe(Arc::new(crate::recipes::reply::Reply));
        r.add_executor(Arc::new(crate::recipes::reply::SendDraft));
        r
    }

    pub fn add_recipe(&mut self, recipe: Arc<dyn Recipe>) {
        self.recipes.insert(recipe.intent(), recipe);
    }

    pub fn add_executor(&mut self, executor: Arc<dyn Executor>) {
        self.executors.insert(executor.action(), executor);
    }

    pub fn recipe(&self, intent: Intent) -> Option<Arc<dyn Recipe>> {
        self.recipes.get(&intent).cloned()
    }

    pub fn executor(&self, action: &str) -> Option<Arc<dyn Executor>> {
        self.executors.get(action).cloned()
    }
}
