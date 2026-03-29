use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockResult};

#[derive(Default)]
pub struct RefCell<T>(RwLock<T>);

impl<T> RefCell<T> {
    pub fn new(val: T) -> Self {
        RefCell(RwLock::new(val))
    }
}

impl<T: Clone> Clone for RefCell<T> {
    fn clone(&self) -> Self {
        RefCell::new(self.0.read().unwrap().clone())
    }
}

impl<T> RefCell<T> {
    pub fn borrow(&self) -> RwLockReadGuard<'_, T> {
        self.0.read().unwrap()
    }
    pub fn borrow_mut(&self) -> RwLockWriteGuard<'_, T> {
        self.0.write().unwrap()
    }
    pub fn try_borrow(&self) -> TryLockResult<RwLockReadGuard<'_, T>> {
        self.0.try_read()
    }
    pub fn try_borrow_mut(&self) -> TryLockResult<RwLockWriteGuard<'_, T>> {
        self.0.try_write()
    }
}
