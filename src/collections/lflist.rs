// usize lock-free, wait free paged linked list stack

use crate::utils::*;
use core::alloc::Alloc;
use core::ptr;
use core::{intrinsics, mem};
use crossbeam::utils::Backoff;
use std::alloc::Global;
use std::intrinsics::size_of;
use std::ops::{Deref, Add};
use std::ptr::null_mut;
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::sync::atomic::{fence, AtomicPtr, AtomicUsize};
use std::borrow::{Borrow, BorrowMut};
use std::cell::{UnsafeCell, Cell};
use crossbeam::atomic::AtomicCell;
use std::hint::unreachable_unchecked;
use rand_xoshiro::Xoroshiro64StarStar;
use std::mem::transmute;
use rand::prelude::*;
use std::cmp::{min, max};

const EMPTY_SLOT: usize = 0;
const SENTINEL_SLOT: usize = 1;
const CACHE_LINE_SIZE: usize = 64;

const EXCHANGE_EMPTY: usize = 0;
const EXCHANGE_WAITING: usize = 1;
const EXCHANGE_BUSY: usize = 2;
const EXCHANGE_SPIN_CYCLES: usize = 1000;

type ExchangeData<T> = Option<(usize, T)>;


struct BufferMeta<T: Default, A: Alloc + Default> {
    head: AtomicUsize,
    next: AtomicPtr<BufferMeta<T, A>>,
    refs: AtomicUsize,
    upper_bound: usize,
    lower_bound: usize,
    tuple_size: usize,
    total_size: usize
}

#[derive(Copy, Clone)]
enum ExchangeDataState<T> {
    Empty,
    Waiting(ExchangeData<T>),
    Busy(ExchangeData<T>)
}

pub struct ExchangeSlot<T: Default + Copy> {
    state: AtomicUsize,
    data: AtomicCell<ExchangeDataState<T>>
}

pub struct ExchangeArray<T: Default + Copy> {
    slots: Vec<ExchangeSlot<T>>
}

pub struct List<T: Default + Copy, A: Alloc + Default = Global> {
    head: AtomicPtr<BufferMeta<T, A>>,
    count: AtomicUsize,
    buffer_cap: usize,
    exchange: ExchangeArray<T>
}

pub struct ThreadLocalRand {
    num: Cell<usize>
}

impl<T: Default + Copy, A: Alloc + Default> List<T, A> {
    pub fn new(buffer_cap: usize) -> Self {
        let first_buffer = BufferMeta::new(buffer_cap);
        Self {
            head: AtomicPtr::new(first_buffer),
            count: AtomicUsize::new(0),
            exchange: ExchangeArray::new(),
            buffer_cap,
        }
    }

    pub fn push(&self, flag: usize, data: T) {
        self.do_push(flag, data);
        self.count.fetch_add(1, Relaxed);
    }

    fn do_push(&self, mut flag: usize, mut data: T) {
        debug_assert_ne!(flag, EMPTY_SLOT);
        debug_assert_ne!(flag, SENTINEL_SLOT);
        loop {
            let obj_size = mem::size_of::<T>();
            let head_ptr = self.head.load(Relaxed);
            let page = BufferMeta::borrow(head_ptr);
            let slot_pos = page.head.load(Relaxed);
            let next_pos = slot_pos + 1;
            if next_pos > self.buffer_cap {
                // buffer overflow, make new and link to last buffer
                let new_head = BufferMeta::new(self.buffer_cap);
                unsafe {
                    (*new_head).next.store(head_ptr, Relaxed);
                }
                if self.head.compare_and_swap(head_ptr, new_head, Relaxed) != head_ptr {
                    BufferMeta::unref(new_head);
                }
            // either case, retry
            } else {
                // in this part, we will try to reason about the push on an buffer
                // It will first try to CAS the head then write the item, finally store a
                // non-zero flag (or value) to the slot.

                // Note that zero in the slot indicates not complete on pop, then pop
                // will back off and try again
                if page.head.compare_and_swap(slot_pos, next_pos, Relaxed) == slot_pos {
                    let slot_ptr = page.flag_ptr_of(slot_pos);
                    unsafe {
                        if obj_size != 0 {
                            let obj_ptr = page.object_ptr_of(slot_ptr);
                            ptr::write(obj_ptr, data);
                        }
                        fence(SeqCst);
                        let slot_flag = intrinsics::atomic_cxchg_relaxed(slot_ptr, EMPTY_SLOT, flag).0;
                        assert_eq!(
                            slot_flag,
                            EMPTY_SLOT,
                            "Cannot swap flag for push. Flag is {} expect empty", slot_flag
                        );
                    }
                    return;
                }
            }
            match self.exchange.exchange(Some((flag, data))) {
                Ok(Some(tuple)) => {
                    // exchanged a push, reset this push parameters
                    flag = tuple.0;
                    data = tuple.1;
                },
                Ok(None) => {
                    // pushed to other popping thread
                    return;
                },
                Err(Some(tuple)) => {
                    // failed exchange, parameters have been returned
                    flag = tuple.0;
                    data = tuple.1;
                }
                Err(None) => {
                    unreachable!();
                }
            }
        }
    }

    pub fn exclusive_push(&self, flag: usize, data: T) {
        // user ensure the push is exclusive, thus no CAS except for header
        let backoff = Backoff::new();
        let obj_size = mem::size_of::<T>();
        loop {
            let head_ptr = self.head.load(Relaxed);
            let page = BufferMeta::borrow(head_ptr);
            let slot_pos = page.head.load(Relaxed);
            let next_pos = slot_pos + 1;
            if next_pos > self.buffer_cap {
                // buffer overflow, make new and link to last buffer
                let new_head = BufferMeta::new(self.buffer_cap);
                unsafe {
                    (*new_head).next.store(head_ptr, Relaxed);
                }
                if self.head.compare_and_swap(head_ptr, new_head, Relaxed) != head_ptr {
                    BufferMeta::unref(new_head);
                }
                // either case, retry
            } else {
                let slot_ptr = page.flag_ptr_of(slot_pos);
                page.head.store(next_pos, Relaxed);
                unsafe {
                    if obj_size != 0 {
                        let obj_ptr = page.object_ptr_of(slot_ptr);
                        ptr::write(obj_ptr, data);
                    }
                    intrinsics::atomic_store_relaxed(slot_ptr, flag);
                }
                self.count.fetch_add(1, Relaxed);
                break;
            }
            backoff.spin();
        }
    }

    pub fn pop(&self) -> Option<(usize, T)> {
        if self.count.load(Relaxed) == 0 {
            return None;
        }
        let backoff = Backoff::new();
        let obj_size = mem::size_of::<T>();
        loop {
            let head_ptr = self.head.load(Relaxed);
            let page = BufferMeta::borrow(head_ptr);
            let slot = page.head.load(Relaxed);
            let obj_size = mem::size_of::<T>();
            let next_buffer_ptr = page.next.load(Relaxed);
            if slot == 0 && next_buffer_ptr == null_mut() {
                // empty buffer chain
                return None;
            }
            if slot == 0 && next_buffer_ptr != null_mut() {
                // last item, need to remove this head and swap to the next one
                // CAS page head to four times of the upper bound indicates this buffer is obsolete
                if self
                    .head
                    .compare_and_swap(head_ptr, next_buffer_ptr, Relaxed)
                    == head_ptr
                {
                    // At thia point, there may have some items in the old head.
                    // Need to check spin wait for other threads to finish working on this buffer
                    // and check head to put back remaining items into the list
                    // This approach may break ordering but we have no other choice here and
                    // the side effect is not significant to its use case
                    drop(page);
                    let dropped_next =
                        BufferMeta::drop_out(head_ptr, &mut Some(|(flag, data)| {
                            if flag != EMPTY_SLOT && flag != SENTINEL_SLOT {
                                self.do_push(flag, data); // push without bump counter
                            }
                        }), &mut 0);
                    debug_assert_eq!(dropped_next.unwrap_or(null_mut()), next_buffer_ptr);
                    // don't need to unref here for drop out did this for us
                }
                continue;
            }
            let mut res = None;
            if slot > 0 {
                unsafe {
                    let new_slot = slot - 1;
                    let new_slot_ptr = page.flag_ptr_of(new_slot);
                    let new_slot_flag = intrinsics::atomic_load_relaxed(new_slot_ptr);
                    if new_slot_flag != 0
                        // first things first, swap the slot to zero if it is not zero
                        && intrinsics::atomic_cxchg_relaxed(new_slot_ptr, new_slot_flag, EMPTY_SLOT).1
                    {
                        res = Some((new_slot_flag, T::default()));
                        if obj_size != 0 && new_slot_flag != SENTINEL_SLOT {
                            res.as_mut()
                                .map(|(_, obj)| unsafe {
                                    let obj_ptr = page.object_ptr_of(new_slot_ptr) as *mut T;
                                    *obj =  ptr::read(obj_ptr as *mut T)
                                });
                        }
                        fence(SeqCst);
                        let swapped = page.head.compare_and_swap(slot, new_slot, Relaxed);
                        debug_assert!(
                            swapped >= slot,
                            "Exclusive pop failed, {} expect {}",
                            swapped,
                            slot
                        );
                        if swapped != slot {
                            // Swap page head failed
                            // The only possible scenario is that there was a push for
                            // pop will back off if flag is detected as zero
                            // In this case, we have a hole in the list, should indicate pop that
                            // this slot does not have any useful information, should pop again
                            intrinsics::atomic_store(new_slot_ptr, SENTINEL_SLOT);
                        }
                        if new_slot_flag != SENTINEL_SLOT {
                            self.count.fetch_sub(1, Relaxed);
                            return res;
                        }
                    }
                }
            } else {
                return res;
            }
            match self.exchange.exchange(None) {
                Ok(Some(tuple)) => {
                    // exchanged a push, return it
                    self.count.fetch_sub(1, Relaxed);
                    return Some(tuple);
                },
                Ok(None) => {
                    // meet another pop
                },
                Err(Some(tuple)) => {
                    unreachable!()
                }
                Err(None) => {
                    // cannot find a pair to exchange
                }
            }
        }
    }
    pub fn drop_out_all<F>(&self, mut retain: Option<F>) where F: FnMut((usize, T)) {
        let count = self.count.load(Relaxed);
        if count == 0 {
            return;
        }
        let pop_threshold = self.buffer_cap >> 2;
        let pop_amount = pop_threshold << 1; // double of the threshold
        let retain = retain.borrow_mut();
        if count < pop_threshold {
            for _ in 0..pop_amount {
                if let Some(pair) = self.pop() {
                    if let Some(retain) = retain {
                        retain(pair);
                    }
                } else {
                    // the only stop condition is that there is no more elements to pop
                    // if it still not empty, continue to swap buffer approach
                    return;
                }
            }
        }
        let new_head_buffer = BufferMeta::new(self.buffer_cap);
        let mut buffer_ptr = self.head.swap(new_head_buffer, Relaxed);
        let null = null_mut();
        let mut counter = 0;
        'main: while buffer_ptr != null {
            buffer_ptr = BufferMeta::drop_out(buffer_ptr, retain, &mut counter).unwrap_or(null);
        }
        self.count.fetch_sub(counter, Relaxed);
    }

    pub fn prepend_with(&self, other: &Self) {
        if other.count.load(Relaxed) == 0 {
            return;
        }
        let other_head = other.head.swap(BufferMeta::new(self.buffer_cap), Relaxed);
        let other_count = other.count.swap(0, Relaxed);
        let mut other_tail = BufferMeta::borrow(other_head);
        // probe the last buffer in other link
        loop {
            while other_tail.refs.load(Relaxed) > 2 {}
            let next_ptr = other_tail.next.load(Relaxed);
            if next_ptr == null_mut() {
                break;
            }
            other_tail = BufferMeta::borrow(next_ptr);
        }

        // CAS this head to other head then reset other tail next buffer to this head
        loop {
            let this_head = self.head.load(Relaxed);
            if self.head.compare_and_swap(this_head, other_head, Relaxed) != this_head {
                continue;
            } else {
                other_tail.next.store(this_head, Relaxed);
                break;
            }
        }
        self.count.fetch_add(other_count, Relaxed);
    }

    pub fn count(&self) -> usize {
        self.count.load(Relaxed)
    }
}

impl<T: Default + Copy, A: Alloc + Default> Drop for List<T, A> {
    fn drop(&mut self) {
        unsafe {
            let mut node_ptr = self.head.load(Relaxed);
            while node_ptr as usize != 0 {
                let next_ptr = (&*node_ptr).next.load(Relaxed);
                BufferMeta::unref(node_ptr);
                node_ptr = next_ptr;
            }
        }
    }
}

impl<T: Default, A: Alloc + Default> BufferMeta<T, A> {
    pub fn new(buffer_cap: usize) -> *mut BufferMeta<T, A> {
        let self_size = mem::size_of::<Self>();
        let meta_size = self_size + align_padding(self_size, CACHE_LINE_SIZE);
        let slots_size = mem::size_of::<usize>();
        let data_size = mem::size_of::<T>();
        let tuple_size = slots_size + data_size;
        let tuple_size_aligned =
            if tuple_size <= 8 { 8 }
            else if tuple_size <= 16 { 16 }
            else if tuple_size <= 32 { 32 }
            else { tuple_size + align_padding(tuple_size, CACHE_LINE_SIZE) };
        let total_size = meta_size + tuple_size_aligned * buffer_cap;
        let head_page = alloc_mem::<T, A>(total_size) as *mut Self;
        let head_page_addr = head_page as usize;
        let slots_start = head_page_addr + meta_size;
        *(unsafe { &mut *head_page }) = Self {
            head: AtomicUsize::new(0),
            next: AtomicPtr::new(null_mut()),
            refs: AtomicUsize::new(1),
            upper_bound: head_page_addr + total_size,
            lower_bound: slots_start,
            tuple_size,
            total_size
        };
        head_page
    }

    pub fn unref(buffer: *mut Self) {
        let rc = {
            let buffer = unsafe { &*buffer };
            buffer.refs.fetch_sub(1, Relaxed)
        };
        if rc == 1 {
            Self::gc(buffer);
        }
    }

    fn gc(buffer: *mut Self) {
        let buffer_ref = unsafe { &*buffer };
        let total_size = buffer_ref.total_size;
        if mem::needs_drop::<T>() {
            Self::flush_buffer(buffer_ref, &mut Some(|x| drop(x)), &mut 0);
        }
        dealloc_mem::<T, A>(buffer as usize, total_size)
    }

    // only use when the buffer is about to be be dead
    // this require reference checking
    fn flush_buffer<F>(buffer: &Self, retain: &mut Option<F>, counter: &mut usize) where F: FnMut((usize, T)) {
        let size_of_obj = mem::size_of::<T>();
        let data_bound = buffer.head.load(Relaxed);
        let mut slot_addr = buffer.lower_bound;
        debug_assert!(
            buffer.refs.load(Relaxed) <= 2 || buffer.refs.load(Relaxed) >= 256,
            "Reference counting check failed"
        );
        for _ in 0..data_bound {
            unsafe {
                let slot = intrinsics::atomic_load_relaxed(slot_addr as *const usize);
                if slot != EMPTY_SLOT && slot != SENTINEL_SLOT {
                    let mut rest = (slot, T::default());
                    if size_of_obj > 0 {
                        rest.1 = ptr::read((slot_addr + mem::size_of::<usize>()) as *const T);
                    }
                    if let Some(retain) = retain {
                        retain(rest);
                    }
                    *counter += 1;
                }
            }
            slot_addr += buffer.tuple_size;
        }
        buffer.head.store(0, Relaxed);
    }

    fn drop_out<F>(
        buffer_ptr: *mut Self,
        retain: &mut Option<F>,
        counter: &mut usize,
    ) -> Option<*mut Self> where F: FnMut((usize, T)) {
        let buffer = BufferMeta::borrow(buffer_ptr);
        let next_ptr = buffer.next.load(Relaxed);
        let backoff = Backoff::new();
        let word_bits = mem::size_of::<usize>() << 3;
        let flag = 1 << (word_bits - 1);
        loop {
            let rc = buffer.refs.load(Relaxed);
            if rc > flag {
                // discovered other drop out, give up
                return None;
            }
            let flag_swap = buffer.refs.compare_and_swap(rc, rc | flag, Relaxed);
            if flag_swap == rc {
                break;
            } else if flag_swap > flag {
                // discovered other drop out, give up
                return None;
            } else {
                backoff.spin();
            }
        }
        loop {
            //wait until reference counter reach 2 one for not garbage one for current reference)
            let rc = buffer.refs.load(SeqCst);
            debug_assert!(rc > flag, "get reference {:x}, value {}", rc, rc & !flag);
            let rc = rc & !flag;
            if rc <= 1 {
                // this buffer is marked to be gc, untouched
                buffer.refs.store(2, Relaxed);
                return Some(next_ptr);
            } else if rc == 2 {
                // no other reference, flush and break out waiting
                buffer.refs.store(rc, Relaxed);
                BufferMeta::flush_buffer(&*buffer, retain, counter);
                BufferMeta::unref(buffer_ptr);
                return Some(next_ptr);
            }
            backoff.spin();
        }
    }

    fn borrow(buffer: *mut Self) -> BufferRef<T, A> {
        {
            let buffer = unsafe { &*buffer };
            buffer.refs.fetch_add(1, Relaxed);
        }
        BufferRef { ptr: buffer }
    }

    fn flag_ptr_of(&self, index: usize) -> *mut usize {
        (self.lower_bound + index * self.tuple_size) as *mut usize
    }

    fn object_ptr_of(&self, flag_ptr: *mut usize) -> *mut T {
        (flag_ptr as usize + mem::size_of::<usize>()) as *mut T
    }
}

struct BufferRef<T: Default, A: Alloc + Default> {
    ptr: *mut BufferMeta<T, A>,
}

impl<T: Default, A: Alloc + Default> Drop for BufferRef<T, A> {
    fn drop(&mut self) {
        BufferMeta::unref(self.ptr);
    }
}

impl<T: Default, A: Alloc + Default> Deref for BufferRef<T, A> {
    type Target = BufferMeta<T, A>;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.ptr }
    }
}

pub struct WordList<A: Alloc + Default = Global> {
    inner: List<(), A>,
}

impl<A: Alloc + Default> WordList<A> {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            inner: List::new(cap),
        }
    }
    pub fn new() -> Self {
        Self::with_capacity(512)
    }
    pub fn push(&self, data: usize) {
        debug_assert_ne!(data, 0);
        debug_assert_ne!(data, 1);
        self.inner.push(data, ())
    }
    pub fn exclusive_push(&self, data: usize) {
        debug_assert_ne!(data, 0);
        debug_assert_ne!(data, 1);
        self.inner.exclusive_push(data, ())
    }
    pub fn pop(&self) -> Option<usize> {
        self.inner.pop().map(|(data, _)| data)
    }

    pub fn drop_out_all<F>(&self, retain: Option<F>) where F: FnMut((usize, ()))  {
        self.inner.drop_out_all(retain);
    }
    pub fn prepend_with(&self, other: &Self) {
        self.inner.prepend_with(&other.inner)
    }
    pub fn count(&self) -> usize {
        self.inner.count()
    }
}

pub struct ObjectList<T: Default + Copy, A: Alloc + Default = Global> {
    inner: List<T, A>,
}

impl<T: Default + Copy, A: Alloc + Default> ObjectList<T, A> {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            inner: List::new(cap),
        }
    }
    pub fn new() -> Self {
        Self::with_capacity(512)
    }
    pub fn push(&self, data: T) {
        self.inner.push(!0, data)
    }
    pub fn exclusive_push(&self, data: T) {
        self.inner.exclusive_push(!0, data)
    }
    pub fn pop(&self) -> Option<T> {
        self.inner.pop().map(|(_, obj)| obj)
    }

    pub fn drop_out_all<F>(&self, retain: Option<F>) where F: Fn((usize, T)) {
        self.inner.drop_out_all(retain)
    }

    pub fn prepend_with(&self, other: &Self) {
        self.inner.prepend_with(&other.inner)
    }
    pub fn count(&self) -> usize {
        self.inner.count()
    }
}

impl <T: Default + Copy> ExchangeSlot<T> {
    fn new() -> Self {
        Self {
            state: AtomicUsize::new(EXCHANGE_EMPTY),
            data: AtomicCell::new(ExchangeDataState::Empty)
        }
    }

    fn exchange(&self, mut data: ExchangeData<T>) -> Result<ExchangeData<T>, ExchangeData<T>> {
        // Memory ordering is somehow important here
        // Will use SeqCst combine with fence for all operations
        let state = self.state.load(SeqCst);
        let backoff = Backoff::new();
        let origin_data_flag = data.as_ref().map(|(f, _)| *f);
        if state == EXCHANGE_EMPTY {
            while !self.data.load().is_empty() {
                // waiting for data to be cleared
            }
            if self.state.compare_and_swap(EXCHANGE_EMPTY, EXCHANGE_WAITING, SeqCst) == EXCHANGE_EMPTY {
                self.data.store(ExchangeDataState::Waiting(data));
                let mut wait_counting = 0;
                loop {
                    // check if it can spin
                    if wait_counting < EXCHANGE_SPIN_CYCLES
                        // if not, CAS to empty, can fail by other thread set BUSY
                        || self.state.compare_and_swap(EXCHANGE_WAITING, EXCHANGE_EMPTY, SeqCst) == EXCHANGE_BUSY
                    {
                        wait_counting += 1;
                        if self.state.load(SeqCst) != EXCHANGE_BUSY {
                            continue;
                        }
                        debug_assert!(!self.data.load().is_empty());
                        while self.data.load().is_waiting() {
                            debug_assert!(!self.data.load().is_empty());
                        } // wait for data to be written
                        debug_assert!(self.data.load().is_busy());
                        let data_result = self.data.swap(ExchangeDataState::Empty);
                        self.state.store(EXCHANGE_EMPTY, SeqCst);
                        if let ExchangeDataState::Busy(res) = data_result {
                            return Ok(res);
                        } else {
                            unreachable!();
                        }
                    } else {
                        // no other thead come and take over, return input
                        assert_eq!(self.state.load(SeqCst), EXCHANGE_EMPTY, "Bad state after bail");
                        let returned_data_state =  self.data.swap(ExchangeDataState::Empty);
                        if let ExchangeDataState::Waiting(returned_data) = returned_data_state {
                            assert_eq!(returned_data.as_ref().map(|(f, _)| *f), origin_data_flag);
                            return Err(returned_data);
                        } else {
                            unreachable!()
                        }
                    }
                }
            } else {
                return Err(data);
            }
        } else if state == EXCHANGE_WAITING {
            // find a pair, get it first
            if self.state.compare_and_swap(EXCHANGE_WAITING, EXCHANGE_BUSY, Relaxed) == EXCHANGE_WAITING {
                while !self.data.load().is_waiting() {
                }
                debug_assert!(self.data.load().is_waiting());
                let data_result = self.data.swap(ExchangeDataState::Busy(data));
                if let ExchangeDataState::Waiting(res) = data_result {
                    return Ok(res);
                } else {
                    unreachable!()
                }
            } else {
                return Err(data);
            }
        } else if state == EXCHANGE_BUSY {
            return Err(data);
        } else {
            unreachable!()
        }
    }
}

unsafe impl <T: Default + Copy> Sync for ExchangeSlot<T> {}
unsafe impl <T: Default + Copy> Send for ExchangeSlot<T> {}

thread_local! {
    static RND: ThreadLocalRand = ThreadLocalRand::new();
}

impl ThreadLocalRand {
    pub fn new() -> Self {
        let mut thread_rng = thread_rng();
        let num = Cell::new(thread_rng.gen());
        Self { num }
    }

    pub fn rand(&self, start: usize, end: usize) -> usize {
        let seed_num = self.num.get();
        let seed = unsafe { transmute(seed_num) };
        self.num.set(seed_num.wrapping_add(1));
        let mut rng = Xoroshiro64StarStar::from_seed(seed);
        rng.gen_range(start, end)
    }
}

impl <T: Default + Copy> ExchangeArray <T> {
    pub fn new() -> Self {
        Self::with_capacity(max(*NUM_CPU >> 4, 2))
    }

    pub fn with_capacity(size: usize) -> Self {
        Self {
            slots: (0..size).map(|_| {
                ExchangeSlot::new()
            }).collect()
        }
    }

    pub fn exchange(&self, data: ExchangeData<T>) -> Result<ExchangeData<T>, ExchangeData<T>> {
        let slot_num = RND.with(|r| r.rand(0, self.slots.len()));
        self.slots[slot_num].exchange(data)
    }
}

impl <T>ExchangeDataState<T> {
    fn is_empty(&self) -> bool {
        match self {
            ExchangeDataState::Empty => true,
            _ => false
        }
    }

    fn is_waiting(&self) -> bool {
        match self {
            ExchangeDataState::Waiting(_) => true,
            _ => false
        }
    }
    fn is_busy(&self) -> bool {
        match self {
            ExchangeDataState::Busy(_) => true,
            _ => false
        }
    }
}

#[cfg(test)]
mod test {
    use crate::collections::lflist::*;
    use crate::utils::SYS_PAGE_SIZE;
    use std::alloc::Global;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::collections::BTreeSet;
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    pub fn general() {
        let list = WordList::<Global>::new();
        let page_size = *SYS_PAGE_SIZE;
        for i in 2..page_size {
            list.push(i);
        }
        for i in (2..page_size).rev() {
            assert_eq!(list.pop(), Some(i));
        }
        for i in 2..page_size {
            assert_eq!(list.pop(), None);
        }
        list.push(32);
        list.push(25);
        assert_eq!(list.count(), 2);
        let mut dropped = vec![];
        list.drop_out_all(Some(|x| { dropped.push(x); }));
        assert_eq!(dropped, vec![(25, ()), (32, ())]);
        assert_eq!(list.count(), 0);
    }

    #[test]
    pub fn parallel_insertion() {

    }

    #[test]
    pub fn parallel() {
        let page_size = *SYS_PAGE_SIZE;
        let list = Arc::new(ObjectList::<usize,Global>::with_capacity(64));
        let mut threads = (2..page_size)
            .map(|i| {
                let list = list.clone();
                thread::spawn(move || {
                    list.push(i);
                })
            })
            .collect::<Vec<_>>();
        for t in threads {
            t.join();
        }

        let mut counter = 0;
        while list.pop().is_some() {
            counter += 1;
        }
        assert_eq!(counter, page_size - 2);

        // push is fine

        for i in 2..page_size {
            list.push(i);
        }
        let recev_list = Arc::new(WordList::<Global>::with_capacity(64));
        threads = (page_size..(page_size * 2))
            .map(|i| {
                let list = list.clone();
                let recev_list = recev_list.clone();
                thread::spawn(move || {
                    if i % 2 == 0 {
                        list.push(i);
                    } else {
                        let pop_val = list.pop().unwrap();
                        recev_list.push(pop_val);
                    }
                })
            })
            .collect::<Vec<_>>();
        for t in threads {
            t.join();
        }

        let mut agg = vec![];
        while let Some(v) = list.pop() {
            agg.push(v);
        }
        while let Some(v) = recev_list.pop() {
            agg.push(v);
        }
        assert_eq!(recev_list.count(), 0, "receive counter not match");
        assert_eq!(list.count(), 0, "origin counter not match");
        let total_insertion = page_size + page_size / 2 - 2;
        assert_eq!(agg.len(), total_insertion, "unmatch before dedup");
        agg.sort();
        agg.dedup_by_key(|k| *k);
        assert_eq!(agg.len(), total_insertion, "unmatch after dedup");
    }

    #[test]
    pub fn exchange() {
        let exchg = Arc::new(ExchangeSlot::new());
        let exchg_1 = exchg.clone();
        let exchg_2 = exchg.clone();
        let attempt_cycles = 10000;
        let sum_board = Arc::new(Mutex::new(BTreeSet::new()));
        let sum_board_1 = sum_board.clone();
        let sum_board_2 = sum_board.clone();
        let hit_count = Arc::new(AtomicUsize::new(0));
        let hit_count_1 = hit_count.clone();
        let hit_count_2 = hit_count.clone();
        assert_eq!(exchg.exchange(Some((0, ()))), Err(Some((0, ()))), "No paring exchange shall return the parameter");
        let th1 = thread::spawn(move || {
            for i in 0..attempt_cycles {
                let res = exchg_2.exchange(Some((i, ())));
                if res.is_ok() {
                    hit_count_2.fetch_add(1, Relaxed);
                }
                assert!(sum_board_2.lock().unwrap().insert(res.unwrap_or_else(|err| err)));
            }
        });
        let th2 = thread::spawn(move || {
            for i in attempt_cycles..attempt_cycles * 2 {
                let res = exchg_1.exchange(Some((i, ())));
                if res.is_ok() {
                    hit_count_1.fetch_add(1, Relaxed);
                }
                assert!(sum_board_1.lock().unwrap().insert(res.unwrap_or_else(|err| err)));
            }
        });
        th1.join();
        th2.join();
        assert!(hit_count.load(Relaxed) > 0);
        assert_eq!(sum_board.lock().unwrap().len(), attempt_cycles * 2);
        for i in 0..attempt_cycles * 2 {
            assert!(sum_board.lock().unwrap().contains(&Some((i, ()))), "expecting {} but not found", i);
        }
    }
}
