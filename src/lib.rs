use io_uring::{
	squeue,
    cqueue,
    Submitter,
	IoUring,
};
use libc:: {
    iovec,
};
use std::{
    io,
    io::Error,
    process,
    os::unix::io::{AsRawFd, RawFd},
    ptr,
    collections::VecDeque,
};

/// struct IoUringProxy
///
/// A library intended for `unsafe`-less usage of io_uring interface.
pub struct IoUringProxy {
    ring: IoUring<squeue::Entry, cqueue::Entry>,
    backlog: VecDeque<squeue::Entry>, // backlog for every submitted non-success io operation 
}

impl IoUringProxy {
    /// Create a new IoUringProxy instance.
    ///
    ///  `entries` - maximum number of entries in io_uring interface
    ///
    ///  `backlog_size` - size of backlog, keeping the unsuccesful submissions
    pub fn new(entries: u32, backlog_size: usize) -> io::Result<Self> {
        let ring = IoUring::new(entries)?;

        Ok(IoUringProxy {
            ring: ring,
            backlog: VecDeque::with_capacity(backlog_size), 
        })
    }

    /// Return a raw IoUring instance.
    pub fn ring(&mut self) -> &mut IoUring {
        &mut self.ring
    }

    /// Initiate and/or complete asynchronous I/O.
    pub fn submit_and_wait(&self, want: usize) -> io::Result<usize> {
        match self.ring.submit_and_wait(want) {
            Ok(nsubmitted) => Ok(nsubmitted),
            Err(ref err) if err.raw_os_error() == Some(libc::EBUSY) => Ok(0),
            Err(err) => Err(err) 
        }
    }

    /// Register in-memory user buffers for I/O with the kernel.
    /// You can use these buffers with the ReadFixed and WriteFixed operations.
    pub fn register_buffers(&self, bufs: &[iovec]) -> io::Result<()> {
        self.ring.submitter().register_buffers(bufs)
    }

    /// Unregister all previously registered buffers.
    pub fn unregister_buffers(&self) -> io::Result<()> {
        self.ring.submitter().unregister_buffers()
    }

    /// Unregister all previously registered files.
    pub fn unregister_files(&self) -> io::Result<()> {
        self.ring.submitter().unregister_files()
    }

    /// Register files for I/O. You can use the registered files with Fixed.
    pub fn register_files(&self, fds: &[RawFd]) -> io::Result<()> {
        self.ring.submitter().register_files(fds)
    }

    /// Synchronize with the submission queue.
    pub fn sq_sync(&mut self) {
        self.ring.submission().sync()
    }

    /// Synchronize with the completion queue.
    pub fn cq_sync(&mut self) {
        self.ring.completion().sync()
    }

    /// Push single sqe.
    ///
    /// If the push operation is unsuccesful for some reason,
    /// insert it into a backlog.
    pub fn push_sqe(&mut self, sqe: &squeue::Entry) {
        unsafe {
            if self.ring.submission().push(sqe).is_err() {
                self.backlog.push_back(sqe.clone());
            }
        }
    }

    /// Push multiple sqes.
    ///
    /// If the push operation is unsuccesful for some reason,
    /// insert those sqes into a backlog.
    pub fn push_multiple_sqes(&mut self, sqes: &[squeue::Entry]) {
        unsafe {
            if self.ring.submission().push_multiple(&sqes).is_err() {
                for sqe in sqes {
                    self.backlog.push_back(sqe.clone());
                }
            }
        }
    }

    /// Return next cqe.
    pub fn cqe_pop(&mut self) -> Option<cqueue::Entry> {
        self.ring.completion().next()
    }

    /// Sched sqes currently residing inside a backlog.
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

