use either::Either;
use instant::Instant;
use leptos::{
    create_memo, create_memo_nodedup, create_render_effect, create_rw_signal, untrack, RwSignal,
    Signal, SignalGet, SignalGetUntracked, SignalSet, SignalSetter, SignalUpdate, SignalWith,
    SignalWithUntracked,
};
use std::{
    cell::RefCell,
    fmt,
    future::Future,
    mem,
    ops::{Deref, DerefMut, Not},
    rc::Rc,
    time::Duration,
};

use concurrency_utils::Task;
use reactive_merge::ReactiveMerge;
use std_ext::shared_box::SharedBox;

use crate::deferred::defer;

pub trait ReadSignalExt:
    SignalWith<Value = <Self as ReadSignalExt>::Inner>
    + SignalWithUntracked<Value = <Self as ReadSignalExt>::Inner>
    + Clone
    + 'static
{
    type Inner;

    #[track_caller]
    fn map<U>(&self, f: impl FnMut(&Self::Inner) -> U + 'static) -> Signal<U> {
        let self_ = self.clone();
        let f = RefCell::new(f);
        // This can probably be switched to a simple function.
        // We originally did this to minimise semantic diff from `sycamore` during a port where `map` was always executed at most once.
        // I don't think this turned out to be actually needed though.
        create_memo_nodedup(move |_| self_.with(|v| untrack(|| f.borrow_mut()(v)))).into()
    }
    #[track_caller]
    fn map_dedup<U>(&self, f: impl FnMut(&Self::Inner) -> U + 'static) -> Signal<U>
    where
        U: PartialEq,
    {
        let self_ = self.clone();
        let f = RefCell::new(f);
        create_memo(move |_| self_.with(|v| untrack(|| f.borrow_mut()(v)))).into()
    }
    #[track_caller]
    fn map_window<U>(
        &self,
        mut f: impl FnMut(Option<&Self::Inner>, &Self::Inner) -> U + 'static,
    ) -> Signal<U>
    where
        Self::Inner: Clone,
    {
        let current_value = self.with_untracked(Clone::clone);
        let ret = create_rw_signal(untrack(|| (f(None, &current_value))));

        let mut old = current_value;
        self.for_each_after_first(move |new| {
            ret.set(untrack(|| f(Some(&old), new)));
            old = new.clone();
        });
        ret.read_only().into()
    }
    #[track_caller]
    fn map_with_prev<U>(
        &self,
        f: impl FnMut(Option<&U>, &Self::Inner) -> U + 'static,
    ) -> Signal<U> {
        let self_ = self.clone();
        let f = RefCell::new(f);
        let ret =
            create_memo_nodedup(move |old| self_.with(|v| untrack(|| f.borrow_mut()(old, v))));
        ret.into()
    }
    #[track_caller]
    fn map_async<Fut>(
        &self,
        mut f: impl FnMut(&Self::Inner) -> Fut + 'static,
    ) -> Signal<Load<Fut::Output>>
    where
        Fut: Future + 'static,
    {
        let ret = create_rw_signal(Load::Loading);
        self.for_each_async(move |value| {
            ret.set(Load::Loading);
            let result = untrack(|| f(value));
            async move { ret.set(Load::Ready(result.await)) }
        });
        ret.into()
    }
    /// Like `ReadSignalExt::map_async`, but it avoids retriggers if the value is available syncronously.
    /// TODO: is this a good idea? One one hand this could save an initial re-render (as the signal is
    /// immediately the correct value), on the other hand it means that now on this logic path there is
    /// a mix of immediate updates and asyncronous ones.
    #[track_caller]
    fn map_maybe_async<Fut>(
        &self,
        mut f: impl FnMut(&Self::Inner) -> Either<Fut::Output, Fut> + 'static,
    ) -> Signal<Load<Fut::Output>>
    where
        Fut: Future + 'static,
    {
        let mut task = None;
        let ret = create_rw_signal(Load::Loading);

        self.for_each(move |value| {
            drop(task.take());
            match untrack(|| f(value)) {
                Either::Left(done) => ret.set(Load::Ready(done)),
                Either::Right(not_done) => {
                    ret.set(Load::Loading);
                    task.replace(Task::new(
                        async move { ret.set(Load::Ready(not_done.await)) },
                    ));
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
        let signal = create_rw_signal(self.with_untracked(Clone::clone));

        // TODO: use futures more directly.
        let mut task: Option<Task<'static>> = None;
        let done = SharedBox::new(false);
        self.for_each_after_first(move |_| {
            if done.get() {
                drop(task.take());
                done.set(false);
            }
            let (done, self_) = (done.clone(), self_.clone());
            task = Some(task.take().unwrap_or_else(move || {
                Task::new(async move {
                    std_ext::task::sleep(duration).await;
                    done.set(true);
                    signal.set(self_.with_untracked(Clone::clone));
                })
            }));
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
        let signal = create_rw_signal(self.with_untracked(Clone::clone));

        // TODO: use futures more directly.

        let mut task: Option<Task<'static>> = None;
        // To remember to cleanup the task.
        let done = SharedBox::new(false);
        // To skip spawning a task if more than `duration` has already passed.
        let last_update = SharedBox::new(Instant::now());
        self.for_each_after_first(move |value| {
            let (self_, done, last_update) = (self_.clone(), done.clone(), last_update.clone());

            if done.get() {
                drop(task.take());
                done.set(false);
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
                    std_ext::task::sleep(duration).await;
                    last_update.set(Instant::now());
                    signal.set(self_.with_untracked(Clone::clone));
                    done.set(true);
                })),
            };
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
    fn skip_if(&self, mut f: impl FnMut(&Self::Inner) -> bool + 'static) -> Signal<Self::Inner>
    where
        Self::Inner: Clone,
    {
        self.keep_if(move |v| !f(v))
    }
    /// Will start with the same value, but then only values matching the provided closures will be *kept*.
    #[track_caller]
    fn keep_if(&self, mut f: impl FnMut(&Self::Inner) -> bool + 'static) -> Signal<Self::Inner>
    where
        Self::Inner: Clone,
    {
        let ret = create_rw_signal(self.with_untracked(Clone::clone));
        self.for_each(move |value| {
            if f(value) {
                ret.set(value.clone());
            }
        });
        ret.into()
    }

    #[track_caller]
    fn not(&self) -> Signal<<Self::Inner as Not>::Output>
    where
        Self::Inner: Clone + Not,
    {
        self.map(|v| v.clone().not())
    }
    #[track_caller]
    fn is(&self, target: Self::Inner) -> Signal<bool>
    where
        Self::Inner: Eq,
    {
        self.map(move |v| v == &target)
    }

    /// Executes the provided closure over each Inner of the signal, *including* the current one.
    #[track_caller]
    fn for_each(&self, f: impl FnMut(&Self::Inner) + 'static) {
        let self_: Self = self.clone();
        let f = RefCell::new(f);
        create_render_effect(move |_| {
            self_.with(|value| untrack(|| f.borrow_mut()(value)));
        });
    }
    /// Executes the provided closure over each Inner of the signal, *excluding* the current one.
    #[track_caller]
    fn for_each_after_first(&self, mut f: impl FnMut(&Self::Inner) + 'static) {
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
    fn for_each_async<Fut>(&self, mut f: impl FnMut(&Self::Inner) -> Fut + 'static)
    where
        Fut: Future<Output = ()> + 'static,
    {
        let mut task = None;
        self.for_each(move |v| {
            drop(task.take());
            untrack(|| task.replace(Some(Task::new(f(v)))));
        });
    }
    /// Runs a function when the signal changes, taking the old and new Inner as arguments
    #[track_caller]
    fn for_each_window(&self, mut f: impl FnMut(&Self::Inner, &Self::Inner) + 'static)
    where
        Self::Inner: Clone,
    {
        let mut old = self.with_untracked(Clone::clone);
        self.for_each_after_first(move |new| {
            untrack(|| f(&old, new));
            old = new.clone();
        });
    }
}
impl<T, Value> ReadSignalExt for T
where
    T: SignalWith<Value = Value> + SignalWithUntracked<Value = Value> + Clone + 'static,
{
    type Inner = Value;
}

pub trait WriteSignalExt:
    ReadSignalExt
    + SignalSet<Value = <Self as ReadSignalExt>::Inner>
    + SignalUpdate<Value = <Self as ReadSignalExt>::Inner>
{
    #[track_caller]
    fn trigger_subscribers(&self) {
        self.update(|_| {}); // Current docs say this always triggers.
    }

    #[track_caller]
    fn set_if_changed(&self, value: Self::Inner)
    where
        Self::Inner: PartialEq,
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
        mut from: impl FnMut(&Self::Inner) -> U + 'static,
        mut to: impl FnMut(&U) -> Self::Inner + 'static,
    ) -> RwSignal<U>
    where
        U: Clone,
    {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum Status {
            Idle,
            ReactingParent,
            ReactingChild,
        }

        let child: RwSignal<U> = create_rw_signal(self.with_untracked(&mut from));

        let lock = SharedBox::new(Status::Idle);

        self.for_each_after_first({
            let lock = lock.clone();
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
        child.for_each_after_first(move |value| match lock.get() {
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
    T: ReadSignalExt<Inner = Value>
        + SignalSet<Value = Value>
        + SignalUpdate<Value = Value>
        + Clone
{
}

pub struct Modify<T: WriteSignalExt> {
    value: Option<<T as ReadSignalExt>::Inner>,
    signal: T,
}
impl<T> fmt::Debug for Modify<T>
where
    T: WriteSignalExt + fmt::Debug,
    <T as ReadSignalExt>::Inner: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Modify")
            .field("value", &self.value)
            .field("signal", &self.signal)
            .finish()
    }
}
impl<T: WriteSignalExt> Deref for Modify<T> {
    type Target = <T as ReadSignalExt>::Inner;

    fn deref(&self) -> &Self::Target {
        self.value.as_ref().unwrap()
    }
}
impl<T: WriteSignalExt> DerefMut for Modify<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value.as_mut().unwrap()
    }
}
impl<T: WriteSignalExt> Drop for Modify<T> {
    fn drop(&mut self) {
        self.signal.set(self.value.take().unwrap());
    }
}

/// Useful to handle an aggregation over a variable (increasing for now) number of signals.
#[derive(Default)]
pub struct SignalBag<I> {
    trigger: RwSignal<()>,
    bag: Rc<RefCell<Vec<Getter<I>>>>,
}
type Getter<I> = Box<dyn Fn() -> I + 'static>;
impl<I: Clone + 'static> SignalBag<I> {
    pub fn new() -> Self {
        Self {
            trigger: create_rw_signal(()),
            bag: Rc::default(),
        }
    }
    pub fn push(&self, signal: impl ReadSignalExt<Inner = I> + 'static) {
        // We make sure future changes trigger an update.
        let trigger = self.trigger;
        signal.for_each_after_first(move |_| trigger.trigger_subscribers());

        self.bag
            .borrow_mut()
            .push(Box::new(move || signal.with(Clone::clone)));
        self.trigger.trigger_subscribers();
    }
    pub fn map<O: 'static>(&self, mut f: impl FnMut(Vec<I>) -> O + 'static) -> Signal<O> {
        let bag = self.bag.clone();
        self.trigger.map(move |&()| {
            let inputs: Vec<_> = bag.borrow().iter().map(|f| f()).collect();
            f(inputs)
        })
    }
}
impl<I: fmt::Debug> fmt::Debug for SignalBag<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalBag")
            .field("trigger", &self.trigger)
            .field("bag_size", &self.bag.borrow().len())
            .finish()
    }
}
impl<I> Clone for SignalBag<I> {
    fn clone(&self) -> Self {
        Self {
            trigger: self.trigger,
            bag: self.bag.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Load<T> {
    Loading,
    Ready(T),
}
