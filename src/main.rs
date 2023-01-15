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

#[derive(Clone)]
enum ServerFsmToken {
    Accept,
    Poll {
        fd: RawFd,
    },
    Recv {
        fd: RawFd,
        buf_idx: usize,
    },
    Send {
        fd: RawFd,
        buf_idx: usize,
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
    pub fn new(fd: RawFd, token: usize, count: usize, capacity: usize) -> ConnectionSlots {
        ConnectionSlots {
            entry: opcode::Accept::new(types::Fd(fd), ptr::null_mut(), ptr::null_mut())
                .build()
                .user_data(token as _),
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
    let mut buf_indices_pool = Vec::with_capacity(32);
    // allocated per request
    let mut buf_pool = Slab::with_capacity(32);
    //
    let mut token_alloc = Slab::with_capacity(32);


    let mut conns = ConnectionSlots::new(server.as_raw_fd(), token_alloc.insert(ServerFsmToken::Accept), 5, 20);
    conns.spawn_slots(2, &mut syscall_proxy);
     
    loop {
        let nsubmitted = syscall_proxy.submit_and_wait(1).unwrap_or_else(|err| {
            eprintln!("Problem while submitting entries: {err}");
            0
        });

        syscall_proxy.cq_sync();

        conns.spawn_slots(1, &mut syscall_proxy);

        while let Some(cqe) = syscall_proxy.cqe_pop() {
            let ret = cqe.result();
            let token_idx = cqe.user_data() as usize;

            if ret < 0 {
                eprintln!("cqe error: {:?}", io::Error::from_raw_os_error(ret));
                continue;
            }

            let token = &mut token_alloc[token_idx];
            match token {
                ServerFsmToken::Accept => {
                    println!("connection accepted!");

                    conns.produce_slot();

                    let fd = ret;
                    let entry = opcode::PollAdd::new(types::Fd(fd), libc::POLLIN as _)
                        .build()
                        .user_data(token_alloc.insert(ServerFsmToken::Poll{fd}) as _);

                    syscall_proxy.push_sqe(&entry);
                }
                ServerFsmToken::Poll{fd} => {
                    println!("poll!");

                    let (buf_idx, buf) = match buf_indices_pool.pop() {
                        Some(buf_idx) => (buf_idx, &mut buf_pool[buf_idx]),
                        // allocate new buffer on a heap
                        None => {
                            let buf = vec![0u8; 4096].into_boxed_slice();
                            let buf_entry = buf_pool.vacant_entry();
                            (buf_entry.key(), buf_entry.insert(buf))
                        }
                    };

                    let entry = opcode::Recv::new(types::Fd(*fd), buf.as_mut_ptr(), buf.len() as _)
                        .build()
                        .user_data(token_idx as _);
                    *token = ServerFsmToken::Recv{fd: *fd, buf_idx};

                    syscall_proxy.push_sqe(&entry);
                }
                ServerFsmToken::Recv{fd, buf_idx} => {
                    if ret == 0 {
                        println!("shutdown");

                        buf_indices_pool.push(*buf_idx);
                        unsafe {
                            libc::close(*fd);
                        }

                        token_alloc.remove(token_idx);
                    } else {
                        println!("recv!");

                        let len = ret as usize;
                        let buf = &buf_pool[*buf_idx];
                        
                        let entry = opcode::Send::new(types::Fd(*fd), buf.as_ptr(), len as _)
                            .build()
                            .user_data(token_idx as _);
                        *token = ServerFsmToken::Send{fd: *fd, buf_idx: *buf_idx, len: len, offset: 0};

                        syscall_proxy.push_sqe(&entry);
                    }
                }
                ServerFsmToken::Send{fd, buf_idx, offset, len} => {
                    println!("send!");
                    let written = *len as usize;

                    let entry = if *offset + written >= *len {
                        buf_indices_pool.push(*buf_idx);

                        let poll_entry = opcode::PollAdd::new(types::Fd(*fd), libc::POLLIN as _)
                            .build()
                            .user_data(token_idx as _);
                        *token = ServerFsmToken::Poll{fd: *fd};
                        
                        poll_entry
                    } else {
                        let off = *offset + written;
                        let length = *len - off;
                        let buf = &buf_pool[*buf_idx][off..];

                        let send_entry = opcode::Send::new(types::Fd(*fd), buf.as_ptr(), length as _)
                            .build()
                            .user_data(token_idx as _);
                        *token = ServerFsmToken::Send{fd: *fd, buf_idx: *buf_idx, offset: off, len: length};

                        send_entry
                    };

                    syscall_proxy.push_sqe(&entry);
                }
            }
        }

        syscall_proxy.sched_backlog();
    }
}
