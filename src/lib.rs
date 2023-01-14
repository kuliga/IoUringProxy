use io_uring::{
    opcode,
	squeue,
    cqueue,
	types,
	IoUring,
};
use std::{
    io,
    io::Error,
    process,
    os::unix::io::{AsRawFd, RawFd},
    ptr,
    collections::VecDeque,
};

pub struct IoUringProxy {
    ring: IoUring<squeue::Entry, cqueue::Entry>,
    backlog: VecDeque<io_uring::squeue::Entry>, // backlog for every submitted non-success io operation 
}

impl IoUringProxy {
    pub fn new(entries: u32, backlog_size: usize) -> io::Result<Self> {
        let ring = IoUring::new(entries)?;

        Ok(IoUringProxy {
            ring: ring,
            backlog: VecDeque::with_capacity(backlog_size), 
        })
    }

    pub fn ring(&mut self) -> &mut IoUring {
        &mut self.ring
    }

    pub fn submit_and_wait(&self, want: usize) -> io::Result<usize> {
        match self.ring.submit_and_wait(want) {
            Ok(nsubmitted) => Ok(nsubmitted),
            Err(ref err) if err.raw_os_error() == Some(libc::EBUSY) => Ok(0),
            Err(err) => Err(err) 
        }
    }

    pub fn sq_sync(&mut self) {
        self.ring.submission().sync()
    }

    pub fn cq_sync(&mut self) {
        self.ring.completion().sync()
    }

    // koniecznie napisac o klauzuli unsafe dlaczego i po co 
    // i zrobic elegancka dokumentacje wygenerowana przez rust doca
    pub fn push_sqe(&mut self, sqe: &squeue::Entry) {
        unsafe {
            if self.ring.submission().push(sqe).is_err() {
                self.backlog.push_back(sqe.clone());
            }
        }
    }

    pub fn push_multiple_sqes(&mut self, sqes: &[squeue::Entry]) {
        unsafe {
            if self.ring.submission().push_multiple(&sqes).is_err() {
                for sqe in sqes {
                    self.backlog.push_back(sqe.clone());
                }
            }
        }
    }

    pub fn cqe_pop(&mut self) -> Option<cqueue::Entry> {
        self.ring.completion().next()
    }

    pub fn sched_backlog(&mut self) {
        loop {
            if self.ring.submission().is_full() {
                match self.ring.submit() {
                    Ok(_) => (),
                    Err(_) => (),
                }
            }

            match self.backlog.pop_front() {
                Some(sqe) => self.push_sqe(&sqe),
                None => break,
            }
            self.sq_sync();
        }
    }
}
