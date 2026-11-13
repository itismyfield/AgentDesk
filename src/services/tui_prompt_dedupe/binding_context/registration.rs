use super::BindingContext;
use std::{cell::RefCell, marker::PhantomData, rc::Rc};

type AfterSpawn = fn(&BindingContext) -> Result<(), String>;
thread_local! {
    static HOOKS: RefCell<Vec<AfterSpawn>> = const { RefCell::new(Vec::new()) };
}

pub(crate) struct Scope {
    _thread: PhantomData<Rc<()>>,
}

pub(crate) fn enter(after_spawn: AfterSpawn) -> Scope {
    HOOKS.with(|hooks| hooks.borrow_mut().push(after_spawn));
    Scope {
        _thread: PhantomData,
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        HOOKS.with(|hooks| {
            hooks.borrow_mut().pop();
        });
    }
}

pub(super) fn complete(context: &BindingContext) -> Result<(), String> {
    let hook = HOOKS.with(|hooks| hooks.borrow().last().copied());
    match hook {
        Some(hook) => hook(context),
        None => Ok(()),
    }
}
