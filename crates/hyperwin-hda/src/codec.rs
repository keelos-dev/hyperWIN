//! The codec on the controller's link, at address 0: an audio function group with one stereo
//! output converter and one pin it plays through, a speaker that is always there.
//!
//!   node 0  the root
//!   node 1  the audio function group
//!   node 2  the output converter (DAC): 16 bits, 48 kHz
//!   node 3  the pin: a built-in speaker, fed by node 2
//!
//! No amplifiers: the guest's volume is applied by the guest to the samples. Every command is
//! answered, the ones that mean nothing here with 0, as a real codec does.

/// What the codec says it is: the ids of QEMU's output-only codec, which guests know.
pub const VENDOR: u32 = 0x1AF4_0012;
const REVISION: u32 = 0x0010_0101;

const GROUP: u8 = 1;
const DAC: u8 = 2;
const PIN: u8 = 3;

/// 16 bits, 48 kHz, and nothing else: what goes on to the clients, so that the guest does the
/// converting, which it is good at.
const PCM: u32 = 1 << 17 | 1 << 6;
/// A speaker built into the machine: no jack to plug, nothing to detect.
const PIN_CONFIG: u32 = 0x9017_0110;

#[derive(Clone, Copy)]
pub struct Codec {
    /// the converter's stream tag and channel, and its format
    stream: u8,
    format: u16,
    pin_control: u8,
    power: [u8; 4],
    unsolicited: [u8; 4],
}

impl Default for Codec {
    fn default() -> Self {
        // 48 kHz, 16 bits, stereo; the pin's output on
        Self { stream: 0, format: 0x0011, pin_control: 0x40, power: [0; 4], unsolicited: [0; 4] }
    }
}

impl Codec {
    /// One command from the controller: codec address, node, verb. None when it is not for
    /// this codec.
    pub fn command(&mut self, command: u32) -> Option<u32> {
        if command >> 28 != 0 {
            return None;
        }
        let node = (command >> 20 & 0x7F) as u8;
        // verbs 0x7.. and 0xF.. have 12 bits and 8 of payload, the others 4 and 16
        let (verb, payload) = match command >> 16 & 0xF {
            0x7 | 0xF => (command >> 8 & 0xFFF, command & 0xFF),
            short => (short << 8, command & 0xFFFF),
        };
        let at = usize::from(node.min(3));
        Some(match (verb, node) {
            (0xF00, _) => parameter(node, payload),
            // the pin's connections: one, the converter
            (0xF02, PIN) => u32::from(DAC),
            // the converter's stream and channel
            (0x706, DAC) => {
                self.stream = payload as u8;
                0
            }
            (0xF06, DAC) => u32::from(self.stream),
            // its format: set, get
            (0x200, DAC) => {
                self.format = payload as u16;
                0
            }
            (0xA00, DAC) => u32::from(self.format),
            (0x707, PIN) => {
                self.pin_control = payload as u8;
                0
            }
            (0xF07, PIN) => u32::from(self.pin_control),
            (0xF1C, PIN) => PIN_CONFIG,
            // "something is plugged in"
            (0xF09, PIN) => 0x8000_0000,
            // power: the state asked for is the state it is in
            (0x705, _) => {
                self.power[at] = payload as u8 & 0x0F;
                0
            }
            (0xF05, _) => u32::from(self.power[at]) << 4 | u32::from(self.power[at]),
            (0x708, _) => {
                self.unsolicited[at] = payload as u8;
                0
            }
            (0xF08, _) => u32::from(self.unsolicited[at]),
            (0xF20, GROUP) => VENDOR,
            // the function group's reset
            (0x7FF, GROUP) => {
                *self = Self::default();
                0
            }
            _ => 0,
        })
    }
}

/// The "get parameter" verb: what a node is and can do.
fn parameter(node: u8, id: u32) -> u32 {
    match (node, id) {
        (0, 0x00) => VENDOR,
        (0, 0x02) => REVISION,
        // the nodes below: first, count
        (0, 0x04) => 1 << 16 | 1,
        (GROUP, 0x04) => 2 << 16 | 2,
        // an audio function group
        (GROUP, 0x05) => 0x01,
        (GROUP, 0x08) => 0x0808,
        (GROUP | DAC, 0x0A) => PCM,
        // stream formats: PCM
        (GROUP | DAC, 0x0B) => 0x01,
        // power states: D0 and D3
        (GROUP, 0x0F) => 0x09,
        // widget capabilities. The converter: an audio output, stereo, with its own formats
        (DAC, 0x09) => 0x0000_0011,
        // the pin: a pin complex, stereo, with a connection list
        (PIN, 0x09) => 0x0040_0101,
        // it can output
        (PIN, 0x0C) => 0x10,
        // its connection list has one entry
        (PIN, 0x0E) => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_guest_walks() {
        let mut c = Codec::default();
        assert_eq!(c.command(0x000F_0000), Some(VENDOR));
        // the root has one node below, node 1, an audio function group with nodes 2 and 3
        assert_eq!(c.command(0x000F_0004), Some(0x0001_0001));
        assert_eq!(c.command(0x001F_0005), Some(1));
        assert_eq!(c.command(0x001F_0004), Some(0x0002_0002));
        // the pin is connected to the converter
        assert_eq!(c.command(0x003F_0200), Some(2));
        // set the converter's format (a 4-bit verb) and stream, and read them back
        assert_eq!(c.command(0x0022_4011), Some(0));
        assert_eq!(c.command(0x002A_0000), Some(0x4011));
        assert_eq!(c.command(0x0027_0650), Some(0));
        assert_eq!(c.command(0x002F_0600), Some(0x50));
        // another codec address: nobody there
        assert_eq!(c.command(0x100F_0000), None);
    }
}
