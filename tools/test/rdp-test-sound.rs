
// ---- appended by tools/test/rdp-test-client.sh ----
// RDPSND for the test client: it takes PCM only (48 kHz, stereo, 16 bits), so what arrives is
// the server's samples as they are, and writes the waves one after the other to the file
// RDP_SOUND_OUT names (raw PCM). Without that variable the waves are counted and dropped.
#[derive(Debug)]
struct Waves {
    formats: Vec<ironrdp::rdpsnd::pdu::AudioFormat>,
    out: Option<std::fs::File>,
}

impl Waves {
    fn new() -> Self {
        let pcm = ironrdp::rdpsnd::pdu::AudioFormat {
            format: ironrdp::rdpsnd::pdu::WaveFormat::PCM,
            n_channels: 2,
            n_samples_per_sec: 48_000,
            n_avg_bytes_per_sec: 192_000,
            n_block_align: 4,
            bits_per_sample: 16,
            data: None,
        };
        let out = std::env::var_os("RDP_SOUND_OUT").and_then(|path| std::fs::File::create(path).ok());
        Self { formats: vec![pcm], out }
    }
}

impl ironrdp::rdpsnd::client::RdpsndClientHandler for Waves {
    fn get_flags(&self) -> ironrdp::rdpsnd::pdu::AudioFormatFlags {
        ironrdp::rdpsnd::pdu::AudioFormatFlags::ALIVE
    }

    fn get_formats(&self) -> &[ironrdp::rdpsnd::pdu::AudioFormat] {
        &self.formats
    }

    fn wave(&mut self, _format: &ironrdp::rdpsnd::pdu::AudioFormat, _ts: u32, data: std::borrow::Cow<'_, [u8]>) {
        use std::io::Write as _;
        if let Some(out) = &mut self.out {
            let _ = out.write_all(&data);
        }
    }

    fn set_volume(&mut self, _: ironrdp::rdpsnd::pdu::VolumePdu) {}

    fn set_pitch(&mut self, _: ironrdp::rdpsnd::pdu::PitchPdu) {}

    fn close(&mut self) {}
}
