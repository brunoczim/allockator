use std::{
    cell::UnsafeCell,
    fmt,
    mem::{self, MaybeUninit},
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

    pub fn alloc(&self) -> Result<AllocHandle<T>, AllocError> {
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
                Ok(_) => {
                    break Ok(AllocHandle { node: node as *const Node<T> })
                },
                Err(new_index) => index = new_index,
            }
        }
    }

    pub unsafe fn dealloc(&self, ptr: AllocHandle<T>) {
        let diff = ptr.node as usize - (ptr::addr_of!(self.nodes[0]) as usize);
        let index = diff / mem::size_of::<Node<T>>();

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
        self.garbage_list.swap(self.null_index(), AcqRel)
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

    unsafe fn prepend(
        &self,
        first_index: usize,
        last_index: usize,
        dest: &AtomicUsize,
    ) {
        let mut dest_first = dest.load(Acquire);
        loop {
            self.nodes[last_index].next.store(dest_first, Relaxed);
            match dest
                .compare_exchange(dest_first, first_index, Release, Relaxed)
            {
                Ok(_) => break,
                Err(new_free_list) => dest_first = new_free_list,
            }
        }
    }

    unsafe fn prepend_free_list(&self, first_index: usize, last_index: usize) {
        self.prepend(first_index, last_index, &self.free_list)
    }

    unsafe fn prepend_garbage_list(
        &self,
        first_index: usize,
        last_index: usize,
    ) {
        self.prepend(first_index, last_index, &self.garbage_list)
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

pub struct AllocHandle<T> {
    node: *const Node<T>,
}

impl<T> fmt::Debug for AllocHandle<T> {
    fn fmt(&self, fmtr: &mut fmt::Formatter) -> fmt::Result {
        fmtr.debug_struct("AllocHandle").field("node", &self.node).finish()
    }
}

impl<T> AllocHandle<T> {
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
            .field("next", &self.next)
            .field("data", &self.data)
            .finish()
    }
}

#[cfg(test)]
mod test {
    use super::SlabAlloc;

    #[test]
    fn alloc_one() {
        let allocator = SlabAlloc::<i32>::new(3);
        let slab_ptr = allocator.alloc().unwrap();

        unsafe {
            slab_ptr.pointer().write(9);
            assert_eq!(*slab_ptr.pointer(), 9);
            *slab_ptr.pointer() = 5;
            assert_eq!(*slab_ptr.pointer(), 5);

            allocator.dealloc(slab_ptr);
        }
    }

    #[test]
    fn alloc_two_concomitant() {
        let allocator = SlabAlloc::<i32>::new(3);
        let slab_ptr_0 = allocator.alloc().unwrap();
        let slab_ptr_1 = allocator.alloc().unwrap();

        unsafe {
            slab_ptr_0.pointer().write(9);
            assert_eq!(*slab_ptr_0.pointer(), 9);
            slab_ptr_1.pointer().write(5);
            assert_eq!(*slab_ptr_1.pointer(), 5);
            assert_eq!(*slab_ptr_0.pointer(), 9);
            *slab_ptr_0.pointer() = 19;
            assert_eq!(*slab_ptr_0.pointer(), 19);
            assert_eq!(*slab_ptr_1.pointer(), 5);
            *slab_ptr_1.pointer() = 10;
            assert_eq!(*slab_ptr_0.pointer(), 19);
            assert_eq!(*slab_ptr_1.pointer(), 10);

            allocator.dealloc(slab_ptr_0);
            allocator.dealloc(slab_ptr_1);
        }
    }

    #[test]
    fn alloc_again() {
        let allocator = SlabAlloc::<i32>::new(3);
        let slab_ptr = allocator.alloc().unwrap();

        unsafe {
            slab_ptr.pointer().write(9);
            assert_eq!(*slab_ptr.pointer(), 9);
            *slab_ptr.pointer() = 5;
            assert_eq!(*slab_ptr.pointer(), 5);

            allocator.dealloc(slab_ptr);
        }

        let slab_ptr = allocator.alloc().unwrap();

        unsafe {
            slab_ptr.pointer().write(-23);
            assert_eq!(*slab_ptr.pointer(), -23);
            *slab_ptr.pointer() = 58;
            assert_eq!(*slab_ptr.pointer(), 58);

            allocator.dealloc(slab_ptr);
        }
    }

    #[test]
    fn alloc_again_interleaving() {
        let allocator = SlabAlloc::<i32>::new(3);
        let slab_ptr_0 = allocator.alloc().unwrap();
        let slab_ptr_1 = allocator.alloc().unwrap();

        unsafe {
            slab_ptr_0.pointer().write(9);
            assert_eq!(*slab_ptr_0.pointer(), 9);
            slab_ptr_1.pointer().write(5);
            assert_eq!(*slab_ptr_1.pointer(), 5);

            allocator.dealloc(slab_ptr_0);
        }

        let slab_ptr_2 = allocator.alloc().unwrap();

        unsafe {
            slab_ptr_2.pointer().write(-23);
            assert_eq!(*slab_ptr_2.pointer(), -23);
            slab_ptr_1.pointer().write(5);
            assert_eq!(*slab_ptr_1.pointer(), 5);

            allocator.dealloc(slab_ptr_1);
            allocator.dealloc(slab_ptr_2);
        }
    }

    #[test]
    fn alloc_at_limit() {
        let allocator = SlabAlloc::<i32>::new(2);
        let slab_ptr_0 = allocator.alloc().unwrap();
        let slab_ptr_1 = allocator.alloc().unwrap();
        allocator.alloc().unwrap_err();

        unsafe {
            slab_ptr_0.pointer().write(9);
            assert_eq!(*slab_ptr_0.pointer(), 9);
            slab_ptr_1.pointer().write(5);
            assert_eq!(*slab_ptr_1.pointer(), 5);
            assert_eq!(*slab_ptr_0.pointer(), 9);

            allocator.dealloc(slab_ptr_0);
        }

        let slab_ptr_2 = allocator.alloc().unwrap();
        allocator.alloc().unwrap_err();

        unsafe {
            slab_ptr_2.pointer().write(19);
            assert_eq!(*slab_ptr_2.pointer(), 19);
            assert_eq!(*slab_ptr_1.pointer(), 5);

            allocator.dealloc(slab_ptr_1);
            allocator.dealloc(slab_ptr_2);
        }

        let slab_ptr_3 = allocator.alloc().unwrap();
        let slab_ptr_4 = allocator.alloc().unwrap();
        allocator.alloc().unwrap_err();

        unsafe {
            slab_ptr_3.pointer().write(119);
            assert_eq!(*slab_ptr_3.pointer(), 119);
            slab_ptr_4.pointer().write(115);
            assert_eq!(*slab_ptr_4.pointer(), 115);
            assert_eq!(*slab_ptr_3.pointer(), 119);

            allocator.dealloc(slab_ptr_3);
            allocator.dealloc(slab_ptr_4);
        }
    }
}
