use anyhow::Context as _;
use blade_graphics as gpu;
use parking_lot::Mutex;
use std::sync::Arc;
use util::ResultExt;

use super::BladePipelines;

pub(super) type BladePipelinesCache =
    Arc<Mutex<Vec<(gpu::TextureFormat, gpu::AlphaMode, u32, Arc<BladePipelines>)>>>;

pub(super) fn get_or_create_cached_pipelines(
    cache: &BladePipelinesCache,
    gpu: &gpu::Context,
    surface_info: gpu::SurfaceInfo,
    path_sample_count: u32,
) -> Arc<BladePipelines> {
    let mut entries = cache.lock();
    if let Some((_, _, _, existing)) = entries.iter().find(|(fmt, alpha, samples, _)| {
        *fmt == surface_info.format
            && *alpha == surface_info.alpha
            && *samples == path_sample_count
    }) {
        return Arc::clone(existing);
    }
    let created = Arc::new(BladePipelines::new(gpu, surface_info, path_sample_count));
    entries.push((
        surface_info.format,
        surface_info.alpha,
        path_sample_count,
        Arc::clone(&created),
    ));
    created
}

#[cfg_attr(target_os = "macos", derive(Clone))]
pub struct BladeContext {
    pub(super) gpu: Arc<gpu::Context>,
    pub(super) pipelines_cache: BladePipelinesCache,
}

impl BladeContext {
    pub fn new() -> anyhow::Result<Self> {
        let device_id_forced = match std::env::var("ZED_DEVICE_ID") {
            Ok(val) => parse_pci_id(&val)
                .context("Failed to parse device ID from `ZED_DEVICE_ID` environment variable")
                .log_err(),
            Err(std::env::VarError::NotPresent) => None,
            err => {
                err.context("Failed to read value of `ZED_DEVICE_ID` environment variable")
                    .log_err();
                None
            }
        };
        let gpu = Arc::new(
            unsafe {
                gpu::Context::init(gpu::ContextDesc {
                    presentation: true,
                    validation: false,
                    device_id: device_id_forced.unwrap_or(0),
                    ..Default::default()
                })
            }
            .map_err(|e| anyhow::anyhow!("{e:?}"))?,
        );
        Ok(Self {
            gpu,
            pipelines_cache: Arc::new(Mutex::new(Vec::new())),
        })
    }
}

fn parse_pci_id(id: &str) -> anyhow::Result<u32> {
    let mut id = id.trim();

    if id.starts_with("0x") || id.starts_with("0X") {
        id = &id[2..];
    }
    let is_hex_string = id.chars().all(|c| c.is_ascii_hexdigit());
    let is_4_chars = id.len() == 4;
    anyhow::ensure!(
        is_4_chars && is_hex_string,
        "Expected a 4 digit PCI ID in hexadecimal format"
    );

    u32::from_str_radix(id, 16).context("parsing PCI ID as hex")
}

#[cfg(test)]
mod tests {
    use super::parse_pci_id;

    #[test]
    fn test_parse_device_id() {
        assert!(parse_pci_id("0xABCD").is_ok());
        assert!(parse_pci_id("ABCD").is_ok());
        assert!(parse_pci_id("abcd").is_ok());
        assert!(parse_pci_id("1234").is_ok());
        assert!(parse_pci_id("123").is_err());
        assert_eq!(
            parse_pci_id(&format!("{:x}", 0x1234)).unwrap(),
            parse_pci_id(&format!("{:X}", 0x1234)).unwrap(),
        );

        assert_eq!(
            parse_pci_id(&format!("{:#x}", 0x1234)).unwrap(),
            parse_pci_id(&format!("{:#X}", 0x1234)).unwrap(),
        );
        assert_eq!(
            parse_pci_id(&format!("{:#x}", 0x1234)).unwrap(),
            parse_pci_id(&format!("{:#X}", 0x1234)).unwrap(),
        );
    }
}
