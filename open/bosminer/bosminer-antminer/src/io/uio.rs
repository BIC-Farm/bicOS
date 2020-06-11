// Copyright (C) 2020  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

//! Simple wrapper around UIO device

use crate::error::{self, ErrorKind};
use failure::ResultExt;
use uio_async;

pub struct Device {
    pub uio: uio_async::UioDevice,
    uio_name: String,
}

pub enum Type {
    Common(usize),
    WorkRx(usize),
    WorkTx(usize),
    Command(usize),
    GlitchMonitor,
}

impl Type {
    fn per_hashchain_instance(&self, name: &'static str, hashboard_idx: usize) -> String {
        assert!(
            hashboard_idx > 0,
            "BUG: hashboard_idx not greater than zero"
        );
        format!("chain{}-{}", hashboard_idx, name)
    }

    fn shared_instance(&self, name: &'static str) -> String {
        format!("miner-{}", name)
    }

    fn to_uio_name(&self) -> String {
        match self {
            &Type::Common(i) => self.per_hashchain_instance("common", i),
            &Type::WorkRx(i) => self.per_hashchain_instance("work-rx", i),
            &Type::WorkTx(i) => self.per_hashchain_instance("work-tx", i),
            &Type::Command(i) => self.per_hashchain_instance("cmd-rx", i),
            &Type::GlitchMonitor => self.shared_instance("glitch-monitor"),
        }
    }
}

impl Device {
    /// Open UIO device of given type for given hashboard
    ///
    /// * `hashboard_idx` - one-based hashboard index (same as connector number:
    ///   connector J8 means `hashboard_idx=8`)
    /// * `uio_type` - type of uio device, determines what IO block to map
    pub fn open(uio_type: Type) -> error::Result<Self> {
        let uio_name = uio_type.to_uio_name();
        let uio = uio_async::UioDevice::open_by_name(&uio_name).with_context(|_| {
            ErrorKind::UioDevice(uio_name.clone(), "cannot find uio device".to_string())
        })?;
        Ok(Self { uio, uio_name })
    }

    pub fn map<T>(&self) -> error::Result<uio_async::UioTypedMapping<T>> {
        let map = self.uio.map_mapping(0).with_context(|_| {
            ErrorKind::UioDevice(self.uio_name.clone(), "cannot map uio device".to_string())
        })?;
        Ok(map.into_typed())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{io, MidstateCount};
    use std::time::Duration;

    /// Read timeout
    const FIFO_READ_TIMEOUT: Duration = Duration::from_millis(5);

    /// Index of chain for testing (must exist and be defined in DTS)
    const TEST_CHAIN_INDEX: usize = 8;

    /// Try opening UIO device.
    /// This test needs properly configured UIO devices for hash-chain 8 in
    /// device-tree so that we have something to open.
    #[test]
    fn test_lookup_uio() {
        Device::open(Type::Command(TEST_CHAIN_INDEX)).expect("TODO: uio open failed");
    }

    /// Try mapping memory from UIO device.
    #[test]
    fn test_map_uio() {
        let _mem: uio_async::UioTypedMapping<u8> = Device::open(Type::Common(TEST_CHAIN_INDEX))
            .expect("TODO: uio open failed")
            .map()
            .expect("TODO: mapping failed");
    }

    /// Try to map memory twice.
    /// This is to check that the UioMapping Drop trait is working: Drop
    /// does perform unmap which drops the Uio fd lock.
    #[test]
    fn test_map_uio_twice_checklock() {
        // haha! this should fail
        let _: uio_async::UioTypedMapping<u8> = Device::open(Type::Common(TEST_CHAIN_INDEX))
            .expect("TODO: uio open failed")
            .map()
            .expect("TODO: mapping failed");
        let _: uio_async::UioTypedMapping<u8> = Device::open(Type::Common(TEST_CHAIN_INDEX))
            .expect("TODO: uio open failed")
            .map()
            .expect("TODO: mapping failed");
    }

    /// Try to map IRQ.
    #[test]
    fn test_map_irq() {
        Device::open(Type::Command(TEST_CHAIN_INDEX)).expect("TODO: uio open failed");
    }

    fn flush_interrupts() {
        // Flush interrupts by IP core re-init
        io::Core::new(TEST_CHAIN_INDEX, MidstateCount::new(1))
            .unwrap()
            .init_and_split()
            .unwrap();
    }

    /// Test that we get IRQ.
    /// Test it on empty tx queue (IRQ always asserted).
    #[test]
    fn test_get_irq() {
        flush_interrupts();
        let uio = Device::open(Type::WorkTx(TEST_CHAIN_INDEX))
            .expect("TODO: uio open failed")
            .uio;
        uio.irq_enable().expect("TODO: irq enable failed");
        let res = uio
            .irq_wait_timeout(FIFO_READ_TIMEOUT)
            .expect("TODO: waiting for timeout failed");
        assert!(res.is_some(), "TODO: expected interrupt");
    }

    /// Test that we get timeout when waiting for IRQ.
    /// Test it on empty rx queue (IRQ always deasserted).
    #[test]
    fn test_get_irq_timeout() {
        flush_interrupts();

        // cmd rx fifo now shouldn't get any interrupts (it's empty)
        let uio = Device::open(Type::WorkRx(TEST_CHAIN_INDEX))
            .expect("TODO: uio open failed")
            .uio;
        uio.irq_enable().expect("TODO: irq enable failed");
        let res = uio
            .irq_wait_timeout(FIFO_READ_TIMEOUT)
            .expect("TODO: waiting for timeout failed");
        assert!(res.is_none(), "BUG: expected timeout");
    }
}
