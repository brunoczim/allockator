use std::{
    collections::HashMap,
    sync::{mpsc, Arc},
    thread,
    thread::{JoinHandle, ThreadId},
    time::Duration,
};

#[derive(Debug, Clone)]
pub struct InputStream {
    buf: Arc<[u8]>,
    cursor: usize,
}

impl InputStream {
    fn new(data: &[u8]) -> Self {
        Self { buf: Arc::from(data), cursor: 0 }
    }

    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let count = (self.buf.len() - self.cursor).min(buf.len());
        buf[.. count]
            .copy_from_slice(&self.buf[self.cursor .. self.cursor + count]);
        buf[count ..].fill(0);
        self.cursor += count;
        count
    }

    pub fn read_u8(&mut self) -> Option<u8> {
        let mut buf = [0];
        if self.read(&mut buf) == 1 {
            Some(buf[0])
        } else {
            None
        }
    }

    pub fn read_u8_zeroing(&mut self) -> u8 {
        let mut buf = [0];
        self.read(&mut buf);
        buf[0]
    }
}

pub trait Machine: Send + Sync + 'static {
    fn cycle(&mut self, opcode: u8, stream: &mut InputStream);
}

pub trait Spawner {
    type Machine: Machine;

    fn spawn(&mut self, stream: &mut InputStream) -> Self::Machine;
}

impl<F, M> Spawner for F
where
    F: FnMut(&mut InputStream) -> M + ?Sized,
    M: Machine,
{
    type Machine = M;

    fn spawn(&mut self, stream: &mut InputStream) -> Self::Machine {
        self(stream)
    }
}

#[derive(Debug, Clone)]
pub struct Config<S> {
    spawner: S,
    cycle_delay: Duration,
    initial_threads: usize,
    min_threads: usize,
    max_threads: usize,
}

impl<S> Default for Config<S>
where
    S: Default,
{
    fn default() -> Self {
        Self::new(S::default())
    }
}

impl<S> Config<S> {
    pub fn new(spawner: S) -> Self {
        let min_threads = 1;
        Self {
            spawner,
            cycle_delay: Duration::from_millis(10),
            min_threads,
            initial_threads: min_threads,
            max_threads: thread::available_parallelism()
                .ok()
                .map_or(0, |par| (par.get() * 2).max(min_threads)),
        }
    }

    pub fn min_threads(self, min_threads: usize) -> Self {
        assert!(min_threads >= 1);
        Self { min_threads, ..self }
    }

    pub fn max_threads(self, max_threads: usize) -> Self {
        Self { max_threads, ..self }
    }

    pub fn initial_threads(self, initial_threads: usize) -> Self {
        Self { initial_threads, ..self }
    }

    pub fn cycle_delay(self, cycle_delay: Duration) -> Self {
        Self { cycle_delay, ..self }
    }

    pub fn run(mut self, data: &[u8])
    where
        S: Spawner,
    {
        assert!(self.min_threads <= self.max_threads);
        assert!(self.initial_threads >= self.min_threads);
        assert!(self.initial_threads <= self.max_threads);

        let mut stream = InputStream::new(data);

        let (sender, receiver) = mpsc::channel();
        let threads = HashMap::new();
        let main = self.spawner.spawn(&mut stream);

        let mut executor =
            Executor { config: self, main, stream, threads, sender, receiver };

        executor.run();
    }
}

#[derive(Debug)]
struct Executor<S, M> {
    config: Config<S>,
    main: M,
    stream: InputStream,
    threads: HashMap<ThreadId, JoinHandle<()>>,
    sender: mpsc::Sender<ThreadId>,
    receiver: mpsc::Receiver<ThreadId>,
}

impl<S> Executor<S, S::Machine>
where
    S: Spawner,
{
    fn init_run(&mut self) {
        while self.threads.len() < self.config.initial_threads {
            self.spawn();
        }
    }

    fn spawn(&mut self) {
        let sender = self.sender.clone();
        let mut machine = self.config.spawner.spawn(&mut self.stream);
        let mut stream = self.stream.clone();
        let join_handle = thread::spawn(move || {
            while let Some(opcode) = stream.read_u8() {
                machine.cycle(opcode, &mut stream);
            }
            sender.send(thread::current().id()).unwrap();
        });
        self.threads.insert(join_handle.thread().id(), join_handle);
    }

    fn wait_messages(&mut self) {
        if let Ok(message) = self.receiver.recv_timeout(self.config.cycle_delay)
        {
            self.threads.remove(&message);
            while let Ok(message) = self.receiver.try_recv() {
                self.threads.remove(&message).unwrap().join().unwrap();
            }
            self.ensure_min_threads();
        }
    }

    fn ensure_min_threads(&mut self) {
        while self.threads.len() < self.config.min_threads {
            self.spawn();
        }
    }

    fn cycle(&mut self, opcode: u8) {
        match opcode % 2 {
            0 => {
                if self.threads.len() < self.config.max_threads {
                    self.spawn();
                }
            },
            1 => {
                if let Some(opcode) = self.stream.read_u8() {
                    self.main.cycle(opcode, &mut self.stream)
                }
            },
            _ => unreachable!(),
        }
    }

    fn exit_run(&mut self) {
        for (_, join_handle) in self.threads.drain() {
            join_handle.join().unwrap();
        }
    }

    fn run(&mut self) {
        self.init_run();

        while let Some(opcode) = self.stream.read_u8() {
            self.cycle(opcode);
            self.wait_messages();
        }

        self.exit_run();
    }
}
