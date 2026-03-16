use std::{
    mem,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone)]
pub struct SharedBox<T> {
    value: Arc<Mutex<T>>,
}

impl<T> SharedBox<T> {
    pub fn new(value: T) -> Self {
        Self {
            value: Arc::new(Mutex::new(value)),
        }
    }

    pub fn get(&self) -> T
    where
        T: Copy,
    {
        *self.value.try_lock().unwrap()
    }
    pub fn get_cloned(&self) -> T
    where
        T: Clone,
    {
        self.value.try_lock().unwrap().clone()
    }
    pub fn take(&self) -> T
    where
        T: Default,
    {
        mem::take(&mut *self.value.try_lock().unwrap())
    }
    pub fn with_mut<U>(&self, f: fn(&T) -> U) -> U {
        f(&*self.value.try_lock().unwrap())
    }

    pub fn set(&self, value: T) {
        *self.value.try_lock().unwrap() = value;
    }

    pub fn from_to(&self, from: &T, to: T)
    where
        T: PartialEq + Clone,
    {
        assert!(&self.get_cloned() == from);
        self.set(to);
    }
}
