use std::{
    net::TcpListener,
    io,
    process,
    os::unix::io::{AsRawFd, RawFd},
    ptr,
    collections::VecDeque,
};
use io_uring::{
    opcode,
	types,
	IoUring,
    squeue,
};
use http_server_iouring_rev::{
//    ConnectionSlots,
    IoUringProxy,
};
use slab::Slab;

enum ServerFsmToken {
    Accept,
    Poll {
        fd: RawFd,
    },
    Read {
        fd: RawFd,
        buf_index: usize,
    },
    Write {
        fd: RawFd,
        buf_index: usize,
        offset: usize,
        len: usize,
    },
}

pub struct ConnectionSlots {
    entry: squeue::Entry, // one accept entry is enough for every connection attempt
    count: usize, // number of connections for server to currently accept
    capacity: usize, // maximum accepted connections number
}

impl ConnectionSlots {
    pub fn new(fd: RawFd, count: usize, capacity: usize) -> ConnectionSlots {
        ConnectionSlots {
            entry: opcode::Accept::new(types::Fd(fd), ptr::null_mut(), ptr::null_mut())
                .build()
                .user_data(0),
            count: count,
            capacity: capacity,
        }
    }
    
    fn consume_slot(&mut self) {
        self.count += 1;
    }

    // should be invoked when the connection is accepted
    pub fn produce_slot(&mut self) {
        self.count -= 1;
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn is_full(&self) -> bool {
        self.count == self.capacity
    }

    pub fn spawn_slots(&mut self, want: usize, proxy: &mut IoUringProxy) {
        for n in 0..want {
            if !self.is_full() {
                proxy.push_sqe(&self.entry);
                self.consume_slot();
            }
        }

        // sync the submission queue with the kernel
        proxy.sq_sync();
    }
}

fn main() {
    let server = TcpListener::bind(("127.0.0.1", 7777)).unwrap_or_else(|err| {
        eprintln!("Problem while binding to the specific socket: {err}");
        process::exit(1);
    });

    // explicit error checking
    if let Ok(addr) = server.local_addr() {
        println!("server listening on: {}", addr);
    } else {
        println!("error getting local socket's address");
    }

    let mut syscall_proxy = IoUringProxy::new(128, 64).unwrap_or_else(|err| { 
        eprintln!("Problem while creating the new ring: {err}");
        process::exit(1);
    });

    // keeps indices of every buffer
    let mut buf_pool = Vec::with_capacity(32);
    //
    let mut buf_alloc = Slab::with_capacity(32);
    //
    let mut token_alloc = Slab::with_capacity(32);


    let mut conns = ConnectionSlots::new(server.as_raw_fd(), 5, 20);
    conns.spawn_slots(2, &mut syscall_proxy);
     
    loop {
        let nsubmitted = syscall_proxy.submit_and_wait(1).unwrap_or_else(|err| {
            eprintln!("Problem while submitting entries: {err}");
            0
        });

        syscall_proxy.cq_sync();

        conns.spawn_slots(1, &mut syscall_proxy);

        while let Some(cqe) = syscall_proxy.cqe_pop() {
            println!("hello");
        }

        syscall_proxy.sched_backlog();
    }
}
