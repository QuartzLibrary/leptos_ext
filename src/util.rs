pub struct SharedBox<T> {
    value: Arc<RwLock<T>>,
}

impl<T> SharedBox<T> {
    pub fn new(value: T) -> Self {
        Self {
            value: Arc::new(RwLock::new(value)),
        }
    }
}
