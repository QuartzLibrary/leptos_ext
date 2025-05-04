use std::{
    mem,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::stream::{AbortHandle, Abortable};

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

pub async fn sleep(duration: Duration) {
    #[cfg(not(target_family = "wasm"))]
    tokio::time::sleep(duration).await;

    #[cfg(target_family = "wasm")]
    {
        let (send, recv) = futures::channel::oneshot::channel();
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::sleep(duration).await;
            let _ = send.send(());
        });
        recv.await.unwrap();
    }
}

// TODO
// pub async fn sleep_until(i: Instant) {
//     #[cfg(not(target_family = "wasm"))]
//     tokio::time::sleep_until(tokio::time::Instant::from_std(i)).await;
//     #[cfg(target_family = "wasm")]
//     gloo_timers::future::sleep_until(i).await;
// }

pub struct Task {
    abort_handle: AbortHandle,
}

impl Task {
    pub fn new(f: impl Future<Output = ()> + Send + 'static) -> Self {
        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        let f = Abortable::new(f, abort_registration);
        let f = async move {
            let _ = f.await;
        };
        leptos::task::spawn(f);
        Self { abort_handle }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}
