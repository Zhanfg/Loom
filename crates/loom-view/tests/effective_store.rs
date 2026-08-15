use loom_map::{LoomMap, ReplacementExtent};
use loom_types::{Sector, SectorCount};
use loom_view::EffectiveBlockStore;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const BLOCK_SIZE: u32 = 4096;
const SECTORS_PER_BLOCK: u64 = 8;
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn fixture(name: &str, blocks: u8) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "loom-view-{name}-{}-{id}.bin",
        std::process::id()
    ));
    let mut file = File::create(&path).unwrap();
    for block in 0..blocks {
        file.write_all(&vec![block; BLOCK_SIZE as usize]).unwrap();
    }
    file.flush().unwrap();
    path
}

#[test]
fn promotion_order_does_not_change_compiled_artifacts() {
    let origin = fixture("order", 8);

    let mut first = EffectiveBlockStore::open(&origin, BLOCK_SIZE).unwrap();
    first.block_mut(5).unwrap()[0] = 0xa5;
    first.block_mut(2).unwrap()[0] = 0xa2;
    let first = first.finalize().unwrap();

    let mut second = EffectiveBlockStore::open(&origin, BLOCK_SIZE).unwrap();
    second.block_mut(2).unwrap()[0] = 0xa2;
    second.block_mut(5).unwrap()[0] = 0xa5;
    let second = second.finalize().unwrap();

    assert_eq!(first.shadow, second.shadow);
    assert_eq!(first.map, second.map);
    assert_eq!(first.shadow_blocks, 2);
    assert_eq!(first.shadow[0], 0xa2);
    assert_eq!(first.shadow[BLOCK_SIZE as usize], 0xa5);
    fs::remove_file(origin).unwrap();
}

#[test]
fn repeated_writes_coalesce_into_one_shadow_block() {
    let origin = fixture("coalesce", 4);
    let mut store = EffectiveBlockStore::open(&origin, BLOCK_SIZE).unwrap();
    store.block_mut(1).unwrap()[0] = 0x11;
    store.block_mut(1).unwrap()[1] = 0x22;
    store.block_mut(1).unwrap()[0] = 0x33;
    assert_eq!(store.dirty_blocks(), 1);

    let compiled = store.finalize().unwrap();
    assert_eq!(compiled.shadow_blocks, 1);
    assert_eq!(&compiled.shadow[..2], &[0x33, 0x22]);
    fs::remove_file(origin).unwrap();
}

#[test]
fn reverting_to_stock_elides_promoted_block() {
    let origin = fixture("revert", 4);
    let mut store = EffectiveBlockStore::open(&origin, BLOCK_SIZE).unwrap();
    let stock = store.read_block(2).unwrap();
    store.block_mut(2).unwrap()[0] ^= 0xff;
    store.block_mut(2).unwrap().copy_from_slice(&stock);

    let compiled = store.finalize().unwrap();
    assert_eq!(compiled.shadow_blocks, 0);
    assert!(compiled.shadow.is_empty());
    assert_eq!(compiled.map.extents().len(), 1);
    fs::remove_file(origin).unwrap();
}

#[test]
fn compiled_shadow_can_be_rehydrated_and_mutated_again() {
    let origin = fixture("rehydrate", 6);
    let mut shadow = vec![0x91_u8; BLOCK_SIZE as usize];
    shadow.extend(vec![0x92_u8; BLOCK_SIZE as usize]);
    let map = LoomMap::from_replacements(
        SectorCount(6 * SECTORS_PER_BLOCK),
        &[
            ReplacementExtent {
                logical_start: Sector(SECTORS_PER_BLOCK),
                sector_count: SectorCount(SECTORS_PER_BLOCK),
                shadow_start: Sector(0),
            },
            ReplacementExtent {
                logical_start: Sector(2 * SECTORS_PER_BLOCK),
                sector_count: SectorCount(SECTORS_PER_BLOCK),
                shadow_start: Sector(SECTORS_PER_BLOCK),
            },
        ],
    )
    .unwrap();

    let mut store = EffectiveBlockStore::from_compiled(&origin, BLOCK_SIZE, &map, &shadow).unwrap();
    assert_eq!(store.dirty_blocks(), 2);
    store.block_mut(2).unwrap()[0] = 0xee;
    store.block_mut(4).unwrap()[0] = 0xe4;

    let compiled = store.finalize().unwrap();
    assert_eq!(compiled.shadow_blocks, 3);
    assert_eq!(compiled.shadow[BLOCK_SIZE as usize], 0xee);
    assert_eq!(compiled.shadow[2 * BLOCK_SIZE as usize], 0xe4);
    fs::remove_file(origin).unwrap();
}
