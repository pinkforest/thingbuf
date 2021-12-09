use crate::{
    loom::{
        atomic::{AtomicBool, AtomicUsize, Ordering::*},
        cell::UnsafeCell,
    },
    util::{mutex::Mutex, CachePadded},
    wait::{Notify, WaitResult},
};

use core::{fmt, marker::PhantomPinned, pin::Pin, ptr::NonNull};

#[derive(Debug)]
pub(crate) struct WaitQueue<T> {
    state: CachePadded<AtomicUsize>,
    list: Mutex<List<T>>,
}

#[derive(Debug)]
pub(crate) struct Waiter<T> {
    node: UnsafeCell<Node<T>>,
    queued: AtomicBool,
}

#[derive(Debug)]
#[pin_project::pin_project]
struct Node<T> {
    next: Link<Waiter<T>>,
    prev: Link<Waiter<T>>,
    waiter: Option<T>,

    // This type is !Unpin due to the heuristic from:
    // <https://github.com/rust-lang/rust/pull/82834>
    #[pin]
    _pin: PhantomPinned,
}

type Link<T> = Option<NonNull<T>>;

struct List<T> {
    head: Link<Waiter<T>>,
    tail: Link<Waiter<T>>,
}

const CLOSED: usize = 1 << 0;
const ONE_QUEUED: usize = 1 << 1;

impl<T: Notify + Unpin> WaitQueue<T> {
    pub(crate) fn new() -> Self {
        Self {
            state: CachePadded(AtomicUsize::new(0)),
            list: Mutex::new(List::new()),
        }
    }

    pub(crate) fn push_waiter(
        &self,
        waiter: &mut Option<Pin<&mut Waiter<T>>>,
        mk_waiter: impl FnOnce() -> T,
    ) -> WaitResult {
        test_println!("WaitQueue::push_waiter()");
        let mut state = test_dbg!(self.state.load(Acquire));
        if test_dbg!(state & CLOSED != 0) {
            return WaitResult::TxClosed;
        }

        if test_dbg!(waiter.is_some()) {
            while test_dbg!(state > CLOSED) {
                match test_dbg!(self.state.compare_exchange_weak(
                    state,
                    state.saturating_sub(ONE_QUEUED),
                    AcqRel,
                    Acquire
                )) {
                    Ok(_) => return WaitResult::Notified,
                    Err(actual) => state = test_dbg!(actual),
                }
            }

            if test_dbg!(state & CLOSED != 0) {
                return WaitResult::TxClosed;
            }

            let mut list = self.list.lock();
            let state = test_dbg!(self.state.load(Acquire));
            if test_dbg!(state >= ONE_QUEUED) {
                self.state
                    .compare_exchange(state, state.saturating_sub(ONE_QUEUED), AcqRel, Acquire)
                    .expect("should succeed");
                return WaitResult::Notified;
            }

            if let Some(mut waiter) = waiter.take() {
                test_println!("WaitQueue::push_waiter -> pushing {:p}", waiter);
                if test_dbg!(waiter.queued.swap(true, Relaxed)) {
                    return WaitResult::Wait;
                }
                waiter.as_mut().node.with_mut(|node| unsafe {
                    let node = &mut *node;
                    node.waiter = Some(mk_waiter());
                    test_println!("-> push {:?}", node);
                });
                list.push_front(waiter);
            } else {
                unreachable!("this could be unchecked...")
            }
        }

        WaitResult::Wait
    }

    pub(crate) fn notify(&self) -> bool {
        test_println!("WaitQueue::notify()");
        if let Some(node) = test_dbg!(self.list.lock().pop_back()) {
            node.notify();
            true
        } else {
            self.state.fetch_add(ONE_QUEUED, Release);
            false
        }
    }

    pub(crate) fn close(&self) {
        test_println!("WaitQueue::close()");
        test_dbg!(self.state.fetch_or(CLOSED, Release));
        let mut list = self.list.lock();
        while let Some(node) = list.pop_back() {
            node.notify();
        }
    }
}

// === impl Waiter ===

impl<T: Notify> Waiter<T> {
    pub(crate) fn new() -> Self {
        Self {
            node: UnsafeCell::new(Node {
                next: None,
                prev: None,
                waiter: None,
                _pin: PhantomPinned,
            }),
            queued: AtomicBool::new(false),
        }
    }

    pub(crate) fn register(&self, mk_waiter: impl FnOnce() -> T) {
        unsafe {
            self.with_node(|node| {
                if node.waiter.is_none() {
                    node.waiter = Some(mk_waiter())
                }
            })
        }
    }

    fn notify(self: Pin<&mut Self>) -> bool {
        unsafe {
            self.with_node(|node| {
                if let Some(waker) = node.waiter.take() {
                    waker.notify();
                    true
                } else {
                    false
                }
            })
        }
    }
}

impl<T> Waiter<T> {
    unsafe fn with_node<U>(&self, f: impl FnOnce(&mut Node<T>) -> U) -> U {
        self.node.with_mut(|node| f(&mut *node))
    }

    unsafe fn set_prev(&mut self, prev: Option<NonNull<Waiter<T>>>) {
        self.node.with_mut(|node| (*node).prev = prev);
    }

    // unsafe fn set_next(&mut self, next: Option<NonNull<Waiter<T>>>) {
    //     self.node.with_mut(|node| (*node).next = next);
    // }

    unsafe fn take_prev(&mut self) -> Option<NonNull<Waiter<T>>> {
        self.node.with_mut(|node| (*node).prev.take())
    }

    unsafe fn take_next(&mut self) -> Option<NonNull<Waiter<T>>> {
        self.node.with_mut(|node| (*node).next.take())
    }
}

impl<T: Notify> Waiter<T> {
    pub(crate) fn remove(self: Pin<&mut Self>, q: &WaitQueue<T>) {
        test_println!("Waiter::remove({:p})", self);

        let mut list = q.list.lock();

        if !test_dbg!(self.queued.swap(false, Relaxed)) {
            test_println!("-> the node was not queued");
            return;
        }

        unsafe {
            list.remove(self);
        }
    }
}

unsafe impl<T: Send> Send for Waiter<T> {}
unsafe impl<T: Send> Sync for Waiter<T> {}

// === impl List ===

impl<T> List<T> {
    fn new() -> Self {
        Self {
            head: None,
            tail: None,
        }
    }

    fn push_front(&mut self, waiter: Pin<&mut Waiter<T>>) {
        unsafe {
            waiter.with_node(|node| {
                node.next = self.head;
                node.prev = None;
            })
        }

        let ptr = unsafe { NonNull::from(Pin::into_inner_unchecked(waiter)) };

        assert_ne!(self.head, Some(ptr), "tried to push the same waiter twice!");
        if let Some(mut head) = self.head {
            unsafe {
                head.as_mut().set_prev(Some(ptr));
            }
        }

        self.head = Some(ptr);
        if self.tail.is_none() {
            self.tail = Some(ptr);
        }
    }

    fn pop_back(&mut self) -> Option<Pin<&mut Waiter<T>>> {
        let mut last = self.tail?;
        test_println!("List::pop_back() -> {:p}", last);

        unsafe {
            let last = last.as_mut();

            let _was_queued = test_dbg!(last.queued.swap(false, Relaxed));
            debug_assert!(_was_queued, "should have been queued!");
            let prev = last.take_prev();

            if let Some(mut prev) = prev {
                prev.as_mut().take_next();
            } else {
                self.head = None;
            }

            self.tail = prev;
            last.take_next();

            Some(Pin::new_unchecked(last))
        }
    }

    unsafe fn remove(&mut self, node: Pin<&mut Waiter<T>>) {
        let node_ref = node.get_unchecked_mut();
        let prev = node_ref.take_prev();
        let next = node_ref.take_next();
        let ptr = NonNull::from(node_ref);

        if let Some(mut prev) = prev {
            prev.as_mut().with_node(|prev| {
                debug_assert_eq!(prev.next, Some(ptr));
                prev.next = next;
            })
        } else if self.head == Some(ptr) {
            self.head = next;
        }

        if let Some(mut next) = next {
            next.as_mut().with_node(|next| {
                debug_assert_eq!(next.prev, Some(ptr));
                next.prev = prev;
            });
        } else if self.tail == Some(ptr) {
            self.tail = prev;
        };
    }
}

impl<T> fmt::Debug for List<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("List")
            .field("head", &self.head)
            .field("tail", &self.tail)
            .finish()
    }
}

unsafe impl<T: Send> Send for List<T> {}
