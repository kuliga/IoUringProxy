use std::{
    net::TcpListener,
    io,
    process,
    os::unix::io::{AsRawFd, RawFd},
    ptr,
    collections::VecDeque,
    str,
    fs::File,
    fs,
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
mod http_lib;
use slab::Slab;

#[derive(Debug, Clone)]
enum ServerFsmToken {
    Accept,
    Poll {
        client_fd: RawFd,
    },
    Recv {
        client_fd: RawFd,
        buf_idx: usize,
    },
    HttpFileRead {
        client_fd: RawFd,
        http_status: usize,
        registered_buf_idx: usize,
    },
    Send {
        client_fd: RawFd,
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

    // register io buffers
    let io_bufs = [
        libc::iovec {
            iov_base: vec![0u8; 512].as_mut_ptr() as *mut libc::c_void,
            iov_len: 512,
        },
        libc::iovec {
            iov_base: vec![0u8; 512].as_mut_ptr() as *mut libc::c_void,
            iov_len: 512,
        }
    ];

    match syscall_proxy.register_buffers(&io_bufs) {
        Ok(_) => println!("io buffers registered!"),
        Err(_) => panic!("io buffers not registed"),
    };

    // register fds
    // cant use File::open() because the fd would be dropped if it went out of
    // scope
    let http_fds: [RawFd; 2] = unsafe {[
        // null terminate the strings as this is passed to raw c syscalls
        libc::open("404.html\0".as_ptr() as *const libc::c_char, libc::O_RDWR) as _,
        libc::open("hello.html\0".as_ptr() as *const libc::c_char, libc::O_RDWR) as _,
    ]};
    println!("fds: {} {} ", http_fds[0], http_fds[1]);

    match syscall_proxy.register_files(&http_fds) {
        Ok(_) => println!("http fds registered!"),
        Err(_) => panic!("http fds not registered!"),
    };

    // keeps indices of every buffer
    let mut buf_indices_pool = Vec::with_capacity(32);
    // allocated per request
    let mut buf_pool = Slab::with_capacity(32);
    //
    let mut token_alloc = Slab::with_capacity(64);


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
                        .user_data(token_alloc.insert(ServerFsmToken::Poll{client_fd: fd}) as _);

                    syscall_proxy.push_sqe(&entry);
                }
                ServerFsmToken::Poll{client_fd} => {
                    println!("poll!");

                    let (buf_idx, buf) = match buf_indices_pool.pop() {
                        Some(buf_idx) => (buf_idx, &mut buf_pool[buf_idx]),
                        // allocate new buffer on a heap
                        None => {
                            let buf = vec![0u8; 4096];//.into_boxed_slice();
                            let buf_entry = buf_pool.vacant_entry();
                            (buf_entry.key(), buf_entry.insert(buf))
                        }
                    };

                    let entry = opcode::Recv::new(types::Fd(*client_fd), buf.as_mut_ptr(), buf.len() as _)
                        .build()
                        .user_data(token_idx as _);
                    *token = ServerFsmToken::Recv{client_fd: *client_fd, buf_idx};

                    syscall_proxy.push_sqe(&entry);
                }
                ServerFsmToken::Recv{client_fd, buf_idx} => {
                    if ret == 0 {
                        println!("shutdown");

                        buf_indices_pool.push(*buf_idx);
                        unsafe {
                            libc::close(*client_fd);
                        }

                        token_alloc.remove(token_idx);
                    } else {
                        println!("recv!");

                        let len = ret as usize;
                        let buf = &mut buf_pool[*buf_idx];

                        // requests will always be valid
                        let req = str::from_utf8(&buf.as_slice()).unwrap();
                        let (http_status, http_file) = http_lib::decode_request(req);
                        let reg_buf_idx = match http_file {
                            "hello.html\0" => 1,
                            _ => 0,
                        };

                        buf_indices_pool.push(*buf_idx);

                        // as for now offset in fd table == offset in buffer table
                        let entry = opcode::ReadFixed::new(
                            types::Fixed(reg_buf_idx as _),
                            io_bufs[reg_buf_idx].iov_base as *mut u8,
                            io_bufs[reg_buf_idx].iov_len as _, reg_buf_idx as _)
                            .build()
                            .user_data(token_idx as _);

                        *token = ServerFsmToken::HttpFileRead{
                            client_fd: *client_fd,
                            http_status: http_status,
                            registered_buf_idx: reg_buf_idx,
                        };

                        syscall_proxy.push_sqe(&entry);
                    }
                }
                ServerFsmToken::HttpFileRead{client_fd, http_status, registered_buf_idx} => {
                    println!("HttpFileRead!");

                    let len = ret as usize;

                    let file_io_buf = unsafe {
                        std::slice::from_raw_parts(io_bufs[*registered_buf_idx].iov_base as *const u8,
                                                   io_bufs[*registered_buf_idx].iov_len)
                    };

                    // this has to be wrapped into a function
                    let (buf_idx, buf) = match buf_indices_pool.pop() {
                        Some(buf_idx) => (buf_idx, &mut buf_pool[buf_idx]),
                        // allocate new buffer on a heap
                        None => {
                            let buf = vec![0u8; 4096];//.into_boxed_slice();
                            let buf_entry = buf_pool.vacant_entry();
                            (buf_entry.key(), buf_entry.insert(buf))
                        }
                    };

                    http_lib::format_response(*http_status, len, file_io_buf, buf);

                    let entry = opcode::Send::new(types::Fd(*client_fd), buf.as_ptr(), buf.len() as _)
                        .build()
                        .user_data(token_idx as _);
                    *token = ServerFsmToken::Send{client_fd: *client_fd, buf_idx: buf_idx, len: len, offset: 0};

                    syscall_proxy.push_sqe(&entry);
                }
                ServerFsmToken::Send{client_fd, buf_idx, offset, len} => {
                    println!("send!");
                    let written = *len as usize;

                    let entry = if *offset + written >= *len {
                        buf_indices_pool.push(*buf_idx);

                        let poll_entry = opcode::PollAdd::new(types::Fd(*client_fd), libc::POLLIN as _)
                            .build()
                            .user_data(token_idx as _);
                        *token = ServerFsmToken::Poll{client_fd: *client_fd};
                        
                        poll_entry
                    } else {
                        let off = *offset + written;
                        let length = *len - off;
                        let buf = &buf_pool[*buf_idx][off..];

                        let send_entry = opcode::Send::new(types::Fd(*client_fd), buf.as_ptr(), length as _)
                            .build()
                            .user_data(token_idx as _);
                        *token = ServerFsmToken::Send{client_fd: *client_fd, buf_idx: *buf_idx, offset: off, len: length};

                        send_entry
                    };

                    syscall_proxy.push_sqe(&entry);
                }
            }
        }
        syscall_proxy.sched_backlog();
    }

    syscall_proxy.unregister_files().unwrap();
    syscall_proxy.unregister_buffers().unwrap();
}

fn print_type_of<T>(_: &T) {
    println!("{}", std::any::type_name::<T>())
}
