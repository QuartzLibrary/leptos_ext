use either::Either;
use leptos::prelude::{
    ArcRwSignal, Effect, ImmediateEffect, Memo, Notify, Set, Signal, Update, With, WithUntracked,
    on_cleanup, untrack,
};
use std::{
    fmt,
    future::Future,
    ops::{Deref, DerefMut, Not},
    sync::{Arc, Mutex},
    time::Duration,
};
use web_time::Instant;

use crate::util;
use crate::util::{SharedBox, Task};

pub trait ReadSignalExt:
    With<Value = <Self as ReadSignalExt>::Inner>
    + WithUntracked<Value = <Self as ReadSignalExt>::Inner>
    + Clone
    + Send
    + Sync
    + 'static
{
    type Inner: Send + Sync + 'static + ?Sized;

    #[track_caller]
    fn map<U>(&self, f: impl FnMut(&Self::Inner) -> U + Send + Sync + 'static) -> Signal<U>
    where
        U: Send + Sync + 'static,
    {
        let self_ = self.clone();
        let f = Mutex::new(f);
        Signal::derive(move || self_.with(|v| untrack(|| f.try_lock().unwrap()(v))))
    }
    #[track_caller]
    fn map_dedup<U>(&self, f: impl FnMut(&Self::Inner) -> U + Send + Sync + 'static) -> Signal<U>
    where
        U: PartialEq + Send + Sync + 'static,
    {
        let self_ = self.clone();
        let f = Mutex::new(f);
        Memo::new(move |_| self_.with(|v| untrack(|| f.try_lock().unwrap()(v)))).into()
    }
    #[track_caller]
    fn map_window<U>(
        &self,
        mut f: impl FnMut(Option<Self::Inner>, &Self::Inner) -> U + Send + Sync + 'static,
    ) -> Signal<U>
    where
        Self::Inner: Clone,
        U: Send + Sync + 'static,
    {
        let self_ = self.clone();
        let f = {
            let mut old = None;
            move |new| {
                let ret = f(old.take(), &new);
                old = Some(new);
                ret
            }
        };
        let f = {
            let f = Mutex::new(f);
            move |v| f.try_lock().unwrap()(v)
        };
        Signal::derive(move || self_.with(|v| untrack(|| f(v.clone()))))
    }
    #[track_caller]
    fn map_with_prev<U>(
        &self,
        mut f: impl FnMut(Option<U>, &Self::Inner) -> U + Send + Sync + 'static,
    ) -> Signal<U>
    where
        U: Clone + Send + Sync + 'static,
    {
        let self_ = self.clone();
        let f = {
            let mut old = None;
            move || {
                let ret = self_.with(|new| untrack(|| f(old.take(), new)));
                old = Some(ret.clone()); // Could avoid this clone with a `Memo` equivalent that doesn't dedup.
                ret
            }
        };
        let f = {
            let f = Mutex::new(f);
            move || f.try_lock().unwrap()()
        };
        Signal::derive(f)
    }
    #[track_caller]
    fn map_async<Fut>(
        &self,
        mut f: impl FnMut(&Self::Inner) -> Fut + Send + Sync + 'static,
    ) -> Signal<Load<Fut::Output>>
    where
        Fut: Future + Send + Sync + 'static,
        Fut::Output: Send + Sync,
    {
        let ret = ArcRwSignal::new(Load::Loading);
        self.for_each_async({
            let ret = ret.clone();
            move |value| {
                ret.set(Load::Loading);
                let result = untrack(|| f(value));
                let ret = ret.clone();
                async move { ret.set(Load::Ready(result.await)) }
            }
        });
        ret.into()
    }
    /// Like `ReadSignalExt::map_async`, but it avoids retriggers if the value is available syncronously.
    #[track_caller]
    fn map_maybe_async<Fut>(
        &self,
        mut f: impl FnMut(&Self::Inner) -> Either<Fut::Output, Fut> + Send + Sync + 'static,
    ) -> Signal<Load<Fut::Output>>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + Sync,
    {
        let mut task = None;
        let ret = ArcRwSignal::new(Load::Loading);

        self.for_each({
            let ret = ret.clone();
            move |value| {
                drop(task.take());
                match untrack(|| f(value)) {
                    Either::Left(done) => ret.set(Load::Ready(done)),
                    Either::Right(not_done) => {
                        let ret = ret.clone();
                        ret.set(Load::Loading);
                        task.replace(Task::new(
                            async move { ret.set(Load::Ready(not_done.await)) },
                        ));
                    }
                }
            }
        });

        ret.into()
    }

    /// Returns a [Signal] bound to this one, but that is updated at most every `duration`.
    ///
    /// This always delays all updates by `duration`, unless an update is already scheduled
    /// in which case they are merged.
    #[track_caller]
    fn rate_limit_trailing(&self, duration: Duration) -> Signal<Self::Inner>
    where
        Self::Inner: Clone,
    {
        let self_ = self.clone();
        let signal = ArcRwSignal::new(self.with_untracked(Clone::clone));

        // TODO: use futures more directly.
        let mut task: Option<Task> = None;
        let done = SharedBox::new(false);
        self.for_each_after_first({
            let signal = signal.clone();
            move |_| {
                if done.take() {
                    drop(task.take());
                }
                let done = done.clone();
                let self_ = self_.clone();
                let signal = signal.clone();
                task = Some(task.take().unwrap_or_else(move || {
                    Task::new(async move {
                        util::sleep(duration).await;
                        done.set(true);
                        signal.set(self_.with_untracked(Clone::clone));
                    })
                }));
            }
        });

        signal.into()
    }

    /// Returns a [Signal] bound to this one, but that is updated at most every `duration`.
    ///
    /// This will optimistically run the leading update, and then delay further updates such that
    /// at least `duration` has passed since the previous one, merging them if more than one comes in.
    fn rate_limit_leading(&self, duration: Duration) -> Signal<Self::Inner>
    where
        Self::Inner: Clone,
    {
        let self_ = self.clone();
        let signal = ArcRwSignal::new(self.with_untracked(Clone::clone));

        // TODO: use futures more directly.

        let mut task: Option<Task> = None;
        // To remember to cleanup the task.
        let done = SharedBox::new(false);
        // To skip spawning a task if more than `duration` has already passed.
        let last_update = SharedBox::new(Instant::now());
        self.for_each_after_first({
            let signal = signal.clone();
            move |value| {
                let self_ = self_.clone();
                let done = done.clone();
                let last_update = last_update.clone();
                let signal = signal.clone();

                if done.take() {
                    drop(task.take());
                }

                let update_immediately = last_update.get().elapsed() > duration;

                task = match task.take() {
                    Some(task) => Some(task),
                    None if update_immediately => {
                        last_update.set(Instant::now());
                        signal.set(value.clone());
                        None
                    }
                    None => Some(Task::new(async move {
                        util::sleep(duration).await;
                        last_update.set(Instant::now());
                        signal.set(self_.with_untracked(Clone::clone));
                        done.set(true);
                    })),
                };
            }
        });

        signal.into()
    }

    #[track_caller]
    fn dedup(&self) -> Signal<Self::Inner>
    where
        Self::Inner: PartialEq + Clone,
    {
        self.map_dedup(Self::Inner::clone)
    }
    /// Will start with the same value, but then any values matching the provided closure will be *skipped*.
    #[track_caller]
    fn skip_if(
        &self,
        mut f: impl FnMut(&Self::Inner) -> bool + Send + Sync + 'static,
    ) -> Signal<Self::Inner>
    where
        Self::Inner: Clone,
    {
        self.keep_if(move |v| !f(v))
    }
    /// Will start with the same value, but then only values matching the provided closures will be *kept*.
    #[track_caller]
    fn keep_if(
        &self,
        mut f: impl FnMut(&Self::Inner) -> bool + Send + Sync + 'static,
    ) -> Signal<Self::Inner>
    where
        Self::Inner: Clone,
    {
        let ret = ArcRwSignal::new(self.with_untracked(Clone::clone));
        self.for_each({
            let ret = ret.clone();
            move |value| {
                if f(value) {
                    ret.set(value.clone());
                }
            }
        });
        ret.into()
    }

    #[track_caller]
    fn not(&self) -> Signal<<Self::Inner as Not>::Output>
    where
        Self::Inner: Clone + Not,
        <Self::Inner as Not>::Output: Send + Sync + 'static,
    {
        self.map(|v| v.clone().not())
    }
    #[track_caller]
    fn is(&self, target: Self::Inner) -> Signal<bool>
    where
        Self::Inner: Eq + Sized,
    {
        self.map(move |v| v == &target)
    }

    /// Executes the provided closure over each Inner of the signal, *including* the current one.
    #[track_caller]
    fn for_each(&self, mut f: impl FnMut(&Self::Inner) + Send + Sync + 'static) {
        let self_: Self = self.clone();
        Effect::new_sync(move || {
            self_.with(|value| untrack(|| f(value)));
        });
    }
    /// Executes the provided closure over each Inner of the signal, *excluding* the current one.
    #[track_caller]
    fn for_each_after_first(&self, mut f: impl FnMut(&Self::Inner) + Send + Sync + 'static) {
        let mut first = true;
        self.for_each(move |value| {
            if first {
                first = false;
            } else {
                f(value);
            }
        });
    }
    /// Executes the provided async closure over each value of the signal, *including* the current one.
    ///
    /// NOTE: if the previous task has not finished by the time a new update arrives, it'll be dropped and cancelled.
    fn for_each_async<Fut>(&self, mut f: impl FnMut(&Self::Inner) -> Fut + Send + Sync + 'static)
    where
        Fut: Future<Output = ()> + Send + Sync + 'static,
    {
        let mut task = None;
        self.for_each(move |v| {
            drop(task.take());
            untrack(|| task.replace(Some(Task::new(f(v)))));
        });
    }
    /// Runs a function when the signal changes, taking the old and new Inner as arguments
    #[track_caller]
    fn for_each_window(&self, mut f: impl FnMut(&Self::Inner, &Self::Inner) + Send + Sync + 'static)
    where
        Self::Inner: Clone,
    {
        let mut old = self.with_untracked(Clone::clone);
        self.for_each_after_first(move |new| {
            untrack(|| f(&old, new));
            old = new.clone();
        });
    }

    /// Executes the provided closure over each Inner of the signal, *including* the current one.
    #[track_caller]
    fn for_each_immediate(&self, mut f: impl FnMut(&Self::Inner) + Send + Sync + 'static) {
        let self_: Self = self.clone();
        let effect = ImmediateEffect::new_mut(move || {
            self_.with(|value| untrack(|| f(value)));
        });
        on_cleanup(move || drop(effect));
    }
    /// Executes the provided closure over each Inner of the signal, *excluding* the current one.
    #[track_caller]
    fn for_each_after_first_immediate(
        &self,
        mut f: impl FnMut(&Self::Inner) + Send + Sync + 'static,
    ) {
        let mut first = true;
        self.for_each_immediate(move |value| {
            if first {
                first = false;
            } else {
                f(value);
            }
        });
    }
    /// Executes the provided async closure over each value of the signal, *including* the current one.
    ///
    /// NOTE: if the previous task has not finished by the time a new update arrives, it'll be dropped and cancelled.
    fn for_each_async_immediate<Fut>(
        &self,
        mut f: impl FnMut(&Self::Inner) -> Fut + Send + Sync + 'static,
    ) where
        Fut: Future<Output = ()> + Send + Sync + 'static,
    {
        let mut task = None;
        self.for_each_immediate(move |v| {
            drop(task.take());
            untrack(|| task.replace(Some(Task::new(f(v)))));
        });
    }
    /// Runs a function when the signal changes, taking the old and new Inner as arguments
    #[track_caller]
    fn for_each_window_immediate(
        &self,
        mut f: impl FnMut(&Self::Inner, &Self::Inner) + Send + Sync + 'static,
    ) where
        Self::Inner: Clone,
    {
        let mut old = self.with_untracked(Clone::clone);
        self.for_each_after_first_immediate(move |new| {
            untrack(|| f(&old, new));
            old = new.clone();
        });
    }
}
impl<T> ReadSignalExt for T
where
    T: With + WithUntracked<Value = <T as With>::Value> + Clone + Send + Sync + 'static,
    <T as With>::Value: Send + Sync,
{
    type Inner = <T as With>::Value;
}

pub trait WriteSignalExt:
    ReadSignalExt
    + Set<Value = <Self as ReadSignalExt>::Inner>
    + Update<Value = <Self as ReadSignalExt>::Inner>
{
    #[track_caller]
    fn set_if_changed(&self, value: Self::Inner)
    where
        Self::Inner: PartialEq + Sized,
    {
        if self.with_untracked(|old| old != &value) {
            self.set(value);
        }
    }
    /// Update the provided value in-place, and trigger the subscribers only if any edit has been done.
    fn update_if_changed(&self, f: impl FnOnce(&mut Self::Inner))
    where
        Self::Inner: PartialEq + Clone,
    {
        let mut new = self.with_untracked(Clone::clone);
        f(&mut new);
        self.set_if_changed(new);
    }

    #[track_caller]
    fn flip(&self)
    where
        Self::Inner: Clone + Not<Output = Self::Inner>,
    {
        self.set(self.with_untracked(Clone::clone).not());
    }
    #[track_caller]
    fn modify(&self) -> Modify<Self>
    where
        Self::Inner: Clone,
    {
        Modify {
            value: Some(self.with_untracked(Clone::clone)),
            signal: self.clone(),
        }
    }

    // TODO: get rid of this by adding derived rw signals? Slices?
    // Here it would be useful to have the rw equivalent of [Signal].
    fn double_bind<U>(
        self,
        mut from: impl FnMut(&Self::Inner) -> U + Send + Sync + 'static,
        mut to: impl FnMut(&U) -> Self::Inner + Send + Sync + 'static,
    ) -> ArcRwSignal<U>
    where
        Self::Inner: Sized,
        U: Clone + Send + Sync + 'static,
    {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum Status {
            Idle,
            ReactingParent,
            ReactingChild,
        }

        let child: ArcRwSignal<U> = ArcRwSignal::new(self.with_untracked(&mut from));

        let lock = SharedBox::new(Status::Idle);

        self.for_each_after_first_immediate({
            let lock = lock.clone();
            let child = child.clone();
            move |value| match lock.get() {
                Status::Idle => {
                    lock.from_to(&Status::Idle, Status::ReactingParent);
                    child.set(from(value));
                    lock.from_to(&Status::ReactingParent, Status::Idle);
                }
                Status::ReactingParent => unreachable!(),
                Status::ReactingChild => {}
            }
        });

        let self_ = self.clone();
        child.for_each_after_first_immediate(move |value| match lock.get() {
            Status::Idle => {
                lock.from_to(&Status::Idle, Status::ReactingChild);
                self_.set(to(value));
                lock.from_to(&Status::ReactingChild, Status::Idle);
            }
            Status::ReactingParent => {}
            Status::ReactingChild => unreachable!(),
        });

        child
    }
}
impl<T, Value> WriteSignalExt for T where
    T: ReadSignalExt<Inner = Value> + Set<Value = Value> + Update<Value = Value> + Clone
{
}

pub struct Modify<T>
where
    T: WriteSignalExt,
    <T as ReadSignalExt>::Inner: Sized,
{
    value: Option<<T as ReadSignalExt>::Inner>,
    signal: T,
}
impl<T> fmt::Debug for Modify<T>
where
    T: WriteSignalExt + fmt::Debug,
    <T as ReadSignalExt>::Inner: fmt::Debug + Sized,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Modify")
            .field("value", &self.value)
            .field("signal", &self.signal)
            .finish()
    }
}
impl<T> Deref for Modify<T>
where
    T: WriteSignalExt,
    <T as ReadSignalExt>::Inner: Sized,
{
    type Target = <T as ReadSignalExt>::Inner;

    fn deref(&self) -> &Self::Target {
        self.value.as_ref().unwrap()
    }
}
impl<T> DerefMut for Modify<T>
where
    T: WriteSignalExt,
    <T as ReadSignalExt>::Inner: Sized,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value.as_mut().unwrap()
    }
}
impl<T> Drop for Modify<T>
where
    T: WriteSignalExt,
    <T as ReadSignalExt>::Inner: Sized,
{
    fn drop(&mut self) {
        self.signal.set(self.value.take().unwrap());
    }
}

/// Useful to handle an aggregation over a variable (increasing for now) number of signals.
#[derive(Default)]
pub struct SignalBag<I> {
    trigger: ArcRwSignal<()>,
    bag: Arc<Mutex<Vec<Getter<I>>>>,
}
type Getter<I> = Box<dyn Fn() -> I + Send + Sync + 'static>;
impl<I: Clone + 'static> SignalBag<I> {
    pub fn new() -> Self {
        Self {
            trigger: ArcRwSignal::new(()),
            bag: Arc::default(),
        }
    }
    pub fn push(&self, signal: impl ReadSignalExt<Inner = I> + 'static) {
        // We make sure future changes trigger an update.
        let trigger = self.trigger.clone();
        signal.for_each_after_first(move |_| trigger.notify());

        self.bag
            .try_lock()
            .unwrap()
            .push(Box::new(move || signal.with(Clone::clone)));
        self.trigger.notify();
    }
    pub fn map<O>(&self, mut f: impl FnMut(Vec<I>) -> O + Send + Sync + 'static) -> Signal<O>
    where
        O: Send + Sync + 'static,
    {
        let bag = self.bag.clone();
        self.trigger.map(move |&()| {
            let inputs: Vec<_> = bag.try_lock().unwrap().iter().map(|f| f()).collect();
            f(inputs)
        })
    }
}
impl<I: fmt::Debug> fmt::Debug for SignalBag<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalBag")
            .field("trigger", &self.trigger)
            .field("bag_size", &self.bag.try_lock().unwrap().len())
            .finish()
    }
}
impl<I> Clone for SignalBag<I> {
    fn clone(&self) -> Self {
        Self {
            trigger: self.trigger.clone(),
            bag: self.bag.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Load<T> {
    Loading,
    Ready(T),
}
impl<T> Load<T> {
    pub fn is_ready(&self) -> bool {
        matches!(self, Load::Ready(_))
    }
    pub fn ready(self) -> Option<T> {
        match self {
            Load::Ready(v) => Some(v),
            Load::Loading => None,
        }
    }
    pub fn as_ref(&self) -> Load<&T> {
        match self {
            Load::Ready(v) => Load::Ready(v),
            Load::Loading => Load::Loading,
        }
    }
}
impl<T> Load<&T> {
    pub fn cloned(self) -> Load<T>
    where
        T: Clone,
    {
        match self {
            Load::Ready(v) => Load::Ready(v.clone()),
            Load::Loading => Load::Loading,
        }
    }
}
