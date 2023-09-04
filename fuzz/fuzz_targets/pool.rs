#![no_main]

use allockator::pool::{AllocHandle, PoolAlloc};
use fuzzsuite::{Config, InputStream, Machine, Spawner};
use libfuzzer_sys::fuzz_target;
use std::{
    cell::OnceCell,
    collections::BTreeMap,
    mem,
    sync::{
        atomic::{AtomicUsize, Ordering::*},
        Arc,
    },
};

#[derive(Debug)]
struct Shared {
    pool: PoolAlloc<u128>,
    count: AtomicUsize,
}

impl Shared {
    fn new(pool: PoolAlloc<u128>) -> Self {
        Self { pool, count: AtomicUsize::new(0) }
    }

    fn alloc(&self) -> Option<AllocHandle<u128>> {
        match self.pool.alloc() {
            Ok(handle) => {
                let prev = self.count.fetch_add(1, Relaxed);
                if prev >= self.pool.capacity() {
                    panic!(
                        "Allocated a slot but no slots available\nAllocated \
                         count: {}\nCapacity: {}\n",
                        prev,
                        self.pool.capacity()
                    );
                }
                Some(handle)
            },
            Err(_) => None,
        }
    }

    unsafe fn dealloc(&self, handle: AllocHandle<u128>) {
        let prev = self.count.fetch_sub(1, Relaxed);
        self.pool.dealloc(handle);
        if prev == 0 || prev > self.pool.capacity() {
            panic!(
                "Deallocating a slot but no slots to be \
                 deallocated\nAllocated count: {}\nCapacity: {}\n",
                prev,
                self.pool.capacity(),
            );
        }
    }
}

#[derive(Debug, Default)]
struct PoolSpawner {
    pool: OnceCell<Arc<Shared>>,
}

impl Spawner for PoolSpawner {
    type Machine = PoolMachine;

    fn spawn(&mut self, stream: &mut InputStream) -> Self::Machine {
        let shared = self.pool.get_or_init(|| {
            let mut capacity_bytes = [0; 2];
            stream.read(&mut capacity_bytes);
            let capacity = usize::from(u16::from_le_bytes(capacity_bytes));
            let pool = PoolAlloc::new(capacity);
            Arc::new(Shared::new(pool))
        });

        PoolMachine::new(shared.clone())
    }
}

#[derive(Debug)]
struct PoolMachine {
    shared: Arc<Shared>,
    values: BTreeMap<u128, Vec<AllocHandle<u128>>>,
}

impl PoolMachine {
    fn new(shared: Arc<Shared>) -> Self {
        Self { values: BTreeMap::new(), shared }
    }
}

impl Machine for PoolMachine {
    fn cycle(&mut self, opcode: u8, stream: &mut InputStream) {
        match opcode % 3 {
            0 => {
                let mut bytes = [0; 16];
                stream.read(&mut bytes);
                let value = u128::from_le_bytes(bytes);

                if let Some(handle) = self.shared.alloc() {
                    unsafe {
                        handle.pointer().write(value);
                    }
                    self.values.entry(value).or_default().push(handle);
                }
            },

            1 => {
                let mut bytes = [0; 16];
                stream.read(&mut bytes);
                let value = u128::from_le_bytes(bytes);

                if let Some((&key, _)) = self.values.range(..= value).next() {
                    let handles = self.values.get_mut(&key).unwrap();
                    let mut remove = true;
                    if let Some(handle) = handles.pop() {
                        remove = handles.is_empty();
                        unsafe {
                            assert_eq!(*handle.pointer(), key);
                        }
                        unsafe {
                            self.shared.dealloc(handle);
                        }
                    }
                    if remove {
                        self.values.remove(&key);
                    }
                }
            },

            2 => {
                let mut bytes = [0; 16];
                stream.read(&mut bytes);
                let value = u128::from_le_bytes(bytes);

                if let Some((&key, _)) = self.values.range(..= value).next() {
                    let handles = self.values.get_mut(&key).unwrap();
                    let mut remove = true;
                    if let Some(handle) = handles.pop() {
                        remove = handles.is_empty();

                        unsafe {
                            assert_eq!(*handle.pointer(), key);
                        }

                        let mut bytes = [0; 16];
                        stream.read(&mut bytes);
                        let value = u128::from_le_bytes(bytes);

                        unsafe {
                            *handle.pointer() = value;
                        }

                        self.values.entry(value).or_default().push(handle);
                    }
                    if remove {
                        self.values.remove(&key);
                    }
                }
            },

            _ => unreachable!(),
        }
    }
}

impl Drop for PoolMachine {
    fn drop(&mut self) {
        for (_, handles) in mem::take(&mut self.values) {
            for handle in handles {
                unsafe {
                    self.shared.dealloc(handle);
                }
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    Config::new(PoolSpawner::default()).run(data);
});
