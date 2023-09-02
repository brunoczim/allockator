use core::fmt;
use std::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    ptr,
    sync::atomic::{AtomicUsize, Ordering::*},
};

#[derive(Debug, Clone)]
pub struct AllocError {
    _priv: (),
}

pub struct SlabAlloc<T> {
    defer_count: AtomicUsize,
    garbage_list: AtomicUsize,
    free_list: AtomicUsize,
    nodes: Box<[Node<T>]>,
}

impl<T> fmt::Debug for SlabAlloc<T> {
    fn fmt(&self, fmtr: &mut fmt::Formatter) -> fmt::Result {
        fmtr.debug_struct("SlabAlloc")
            .field("defer_count", &self.defer_count)
            .field("garbage_list", &self.garbage_list)
            .field("free_list", &self.free_list)
            .field("nodes", &self.nodes)
            .finish()
    }
}

impl<T> SlabAlloc<T> {
    pub fn new(capacity: usize) -> Self {
        let nodes = (0 .. capacity).map(|i| Node {
            next: AtomicUsize::new(i + 1),
            data: UnsafeCell::new(MaybeUninit::uninit()),
        });

        Self {
            defer_count: AtomicUsize::new(0),
            garbage_list: AtomicUsize::new(capacity),
            free_list: AtomicUsize::new(0),
            nodes: nodes.collect(),
        }
    }

    pub fn defer_deallocs(&self) -> DeallocDeferral<T> {
        self.acquire_defer();
        DeallocDeferral { allocator: self }
    }

    pub fn try_flush_deallocs(&self) -> bool {
        let garbage_list = if self.defer_hint() == 1 {
            self.take_garbage_list()
        } else {
            self.null_index()
        };

        if self.count_defers() == 0 {
            unsafe { self.free_garbage_list(garbage_list) }
            true
        } else {
            unsafe { self.merge_garbage_list(garbage_list) }
            false
        }
    }

    pub fn flush_deallocs(&mut self) {
        let garbage_list = self.take_garbage_list();
        unsafe { self.free_garbage_list(garbage_list) }
    }

    pub fn alloc(&self) -> Result<SlabPtr<T>, AllocError> {
        let _defer = self.defer_deallocs();
        let mut index = self.free_list.load(Acquire);
        loop {
            let node = match self.nodes.get(index) {
                Some(node) => node,
                None => break Err(AllocError { _priv: () }),
            };
            let next = node.next.load(Relaxed);
            match self.free_list.compare_exchange(index, next, Release, Relaxed)
            {
                Ok(_) => break Ok(SlabPtr { node: node as *const Node<T> }),
                Err(new_index) => index = new_index,
            }
        }
    }

    pub unsafe fn dealloc(&self, ptr: SlabPtr<T>) {
        let index = ptr.node as usize - (ptr::addr_of!(self.nodes[0]) as usize);

        if self.try_flush_deallocs() {
            self.prepend_free_list(index, index);
        } else {
            self.prepend_garbage_list(index, index);
        }
    }

    #[inline]
    fn null_index(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    fn defer_hint(&self) -> usize {
        self.defer_count.load(Relaxed)
    }

    #[inline]
    fn count_defers(&self) -> usize {
        self.defer_count.load(Acquire)
    }

    fn acquire_defer(&self) -> usize {
        self.defer_count.fetch_add(1, Acquire) + 1
    }

    fn release_defer(&self) -> usize {
        self.defer_count.fetch_sub(1, Release) - 1
    }

    fn take_garbage_list(&self) -> usize {
        self.garbage_list.swap(0, AcqRel)
    }

    fn find_last_node(&self, first_index: usize) -> Option<usize> {
        let mut index = first_index;
        let mut last_index = None;
        while let Some(node) = self.nodes.get(index) {
            last_index = Some(index);
            index = node.next.load(Relaxed);
        }
        last_index
    }

    unsafe fn prepend_free_list(&self, first_index: usize, last_index: usize) {
        let mut free_list = self.free_list.load(Acquire);
        loop {
            self.nodes[last_index].next.store(free_list, Relaxed);
            match self
                .free_list
                .compare_exchange(free_list, first_index, Release, Relaxed)
            {
                Ok(_) => break,
                Err(new_free_list) => free_list = new_free_list,
            }
        }
    }

    unsafe fn prepend_garbage_list(
        &self,
        first_index: usize,
        last_index: usize,
    ) {
        let mut garbage_list = self.garbage_list.load(Acquire);
        loop {
            self.nodes[last_index].next.store(garbage_list, Relaxed);
            match self
                .garbage_list
                .compare_exchange(garbage_list, first_index, Release, Relaxed)
            {
                Ok(_) => break,
                Err(new_garbage_list) => garbage_list = new_garbage_list,
            }
        }
    }

    unsafe fn free_garbage_list(&self, first_index: usize) {
        if let Some(last_index) = self.find_last_node(first_index) {
            self.prepend_free_list(first_index, last_index);
        }
    }

    unsafe fn merge_garbage_list(&self, first_index: usize) {
        if let Some(last_index) = self.find_last_node(first_index) {
            self.prepend_garbage_list(first_index, last_index);
        }
    }
}

pub struct SlabPtr<T> {
    node: *const Node<T>,
}

impl<T> fmt::Debug for SlabPtr<T> {
    fn fmt(&self, fmtr: &mut fmt::Formatter) -> fmt::Result {
        fmtr.debug_struct("SlabPtr").field("node", &self.node).finish()
    }
}

impl<T> SlabPtr<T> {
    pub unsafe fn pointer(&self) -> *mut T {
        (*(*self.node).data.get()).as_mut_ptr()
    }
}

pub struct DeallocDeferral<'alloc, T> {
    allocator: &'alloc SlabAlloc<T>,
}

impl<'alloc, T> fmt::Debug for DeallocDeferral<'alloc, T> {
    fn fmt(&self, fmtr: &mut fmt::Formatter) -> fmt::Result {
        fmtr.debug_struct("DeallocDeferral")
            .field("allocator", &self.allocator)
            .finish()
    }
}

impl<'alloc, T> Drop for DeallocDeferral<'alloc, T> {
    fn drop(&mut self) {
        let garbage_list = if self.allocator.defer_hint() == 1 {
            self.allocator.take_garbage_list()
        } else {
            self.allocator.null_index()
        };

        if self.allocator.release_defer() == 0 {
            unsafe { self.allocator.free_garbage_list(garbage_list) }
        } else {
            unsafe { self.allocator.merge_garbage_list(garbage_list) }
        }
    }
}

struct Node<T> {
    next: AtomicUsize,
    data: UnsafeCell<MaybeUninit<T>>,
}

impl<T> fmt::Debug for Node<T> {
    fn fmt(&self, fmtr: &mut fmt::Formatter) -> fmt::Result {
        fmtr.debug_struct("SlabAlloc")
            .field("next_bits", &self.next)
            .field("data", &self.data)
            .finish()
    }
}
