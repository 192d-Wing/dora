//! functions generated to interact with ioctl
//!
#![allow(missing_docs)]

use std::{io, net::Ipv4Addr, os::unix::prelude::AsRawFd};

use dhcproto::v4;
use socket2::SockRef;

/// calls ioctl(fd, SIOCSARP, arpreq) to set `arpreq` in ARP cache
///
/// # Safety
/// fd must be a valid v4 socket.
///
pub fn arp_set(
    soc: SockRef<'_>,
    yiaddr: Ipv4Addr,
    htype: v4::HType,
    chaddr: &[u8],
) -> io::Result<()> {
    // `sa_data` in `sockaddr` is 14 bytes. A client-supplied hardware address
    // longer than that cannot be represented in an ARP request, and copying it
    // in would overflow the buffer. Reject instead of panicking; the caller
    // falls back to broadcasting the response.
    if chaddr.len() > 14 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "hardware address too long for ARP request",
        ));
    }
    let addr_in = libc::sockaddr_in {
        sin_family: libc::AF_INET as _,
        sin_port: v4::CLIENT_PORT.to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(yiaddr.octets()),
        },
        ..unsafe { std::mem::zeroed() }
    };
    // memcpy to sockaddr for arp_req. sockaddr_in and sockaddr both 16 bytes
    let arp_pa: libc::sockaddr = unsafe { std::mem::transmute(addr_in) };
    // create arp_ha (for hardware addr)
    let arp_ha = libc::sockaddr {
        sa_family: u8::from(htype) as _,
        sa_data: unsafe { super::ioctl::cpy_bytes::<14>(chaddr) },
    };

    let arp_req = libc::arpreq {
        arp_pa,
        arp_ha,
        arp_flags: libc::ATF_COM,
        // this line may or may not be necessary? dnsmasq does it but it seems to work without
        // arp_dev: unsafe { super::ioctl::cpy_bytes::<16>(device.as_bytes()) },
        ..unsafe { std::mem::zeroed() }
    };

    // conversion needed for musl target
    #[cfg(not(target_env = "musl"))]
    let siocsarp = libc::SIOCSARP;
    #[cfg(target_env = "musl")]
    let siocsarp = libc::SIOCSARP.try_into().unwrap();

    let res = unsafe { libc::ioctl(soc.as_raw_fd(), siocsarp, &arp_req as *const libc::arpreq) };
    if res == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// # Returns
/// A zeroed out array of size `N` with up to the first `N` `bytes` copied in.
/// If `bytes.len() > N` the extra bytes are ignored (truncated) rather than
/// causing a panic.
///
/// # Safety
/// will create a new slice of `&[libc::c_char]` from the bytes.
pub unsafe fn cpy_bytes<const N: usize>(bytes: &[u8]) -> [libc::c_char; N] {
    unsafe {
        let mut sa_data = [0; N];
        // clamp so an oversized hardware address can never overflow `sa_data`
        let len = bytes.len().min(N);

        sa_data[..len].copy_from_slice(std::slice::from_raw_parts(
            bytes.as_ptr() as *const libc::c_char,
            len,
        ));
        sa_data
    }
}
