//! Freeze exact corpus bytes once so remote machines need no source checkout.
use crate::corpus::Dataset;
use serde::{Deserialize, Serialize};
use std::{error::Error, fs, io::Write, path::Path};

const MAGIC: &[u8; 8] = b"FSBENCH1";
const MAX_BYTES: usize = 32 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
struct Entry {
    name: String,
    description: String,
    provenance: String,
    lengths: Vec<usize>,
}

pub fn write(path: &Path, datasets: &[Dataset]) -> Result<(), Box<dyn Error>> {
    let metadata: Vec<_> = datasets
        .iter()
        .map(|d| Entry {
            name: d.name.clone(),
            description: d.description.clone(),
            provenance: d.provenance.clone(),
            lengths: d.blocks.iter().map(Vec::len).collect(),
        })
        .collect();
    let metadata = serde_json::to_vec(&metadata)?;
    let mut file = fs::File::create(path)?;
    file.write_all(MAGIC)?;
    file.write_all(&u32::try_from(metadata.len())?.to_le_bytes())?;
    file.write_all(&metadata)?;
    for dataset in datasets {
        for block in &dataset.blocks {
            file.write_all(block)?;
        }
    }
    Ok(())
}

pub fn read(path: &Path) -> Result<Vec<Dataset>, Box<dyn Error>> {
    if fs::metadata(path)?.len() > MAX_BYTES as u64 {
        return Err("oversized frozen corpus".into());
    }
    let data = fs::read(path)?;
    if data.len() < 12 || &data[..8] != MAGIC {
        return Err("invalid frozen corpus header".into());
    }
    let metadata_len = u32::from_le_bytes(data[8..12].try_into()?) as usize;
    if metadata_len > 1024 * 1024 {
        return Err("oversized corpus metadata".into());
    }
    let metadata_end = 12 + metadata_len;
    let metadata: Vec<Entry> = serde_json::from_slice(
        data.get(12..metadata_end)
            .ok_or("truncated corpus metadata")?,
    )?;
    if metadata.is_empty() || metadata.len() > 100 {
        return Err("invalid corpus dataset count".into());
    }
    let mut offset = metadata_end;
    let mut datasets = Vec::new();
    for entry in metadata {
        if entry.lengths.is_empty() || entry.lengths.len() > 4096 {
            return Err("invalid corpus block count".into());
        }
        let mut blocks = Vec::new();
        for length in entry.lengths {
            if length == 0 || length > 128 * 1024 {
                return Err("invalid corpus block size".into());
            }
            let end = offset.checked_add(length).ok_or("corpus offset overflow")?;
            blocks.push(
                data.get(offset..end)
                    .ok_or("truncated corpus payload")?
                    .to_vec(),
            );
            offset = end;
        }
        datasets.push(Dataset {
            name: entry.name,
            description: entry.description,
            provenance: entry.provenance,
            blocks,
        });
    }
    if offset != data.len() {
        return Err("trailing frozen corpus bytes".into());
    }
    Ok(datasets)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_bytes_roundtrip_and_truncation_rejected() {
        let path =
            std::env::temp_dir().join(format!("flowsplice-corpus-test-{}.bin", std::process::id()));
        let inputs = vec![Dataset {
            name: "test".into(),
            description: "binary bytes".into(),
            provenance: "fixture".into(),
            blocks: vec![vec![0, 255, 13, 10], vec![1; 4096]],
        }];
        write(&path, &inputs).unwrap();
        let decoded = read(&path).unwrap();
        assert_eq!(decoded[0].blocks, inputs[0].blocks);
        assert_eq!(decoded[0].provenance, inputs[0].provenance);
        let mut bytes = fs::read(&path).unwrap();
        bytes.pop();
        fs::write(&path, bytes).unwrap();
        assert!(read(&path).is_err());
        fs::remove_file(path).unwrap();
    }
}
