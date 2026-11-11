//! Power-loss behaviour, which only the simulated disk can show.

use castcorder_port_disk::{SimDisk, Storage, StorageFile};

fn contents(disk: &SimDisk, name: &str) -> Vec<u8> {
    let mut f = disk.open(name).unwrap();
    let mut buf = vec![0u8; f.len().unwrap() as usize];
    f.read_at(0, &mut buf).unwrap();
    buf
}

#[test]
fn synced_data_survives_and_unsynced_is_lost_or_torn() {
    for seed in 0..50 {
        let disk = SimDisk::new(seed);
        let mut f = disk.open("t.wav").unwrap();
        f.write_at(0, b"durable").unwrap();
        f.sync().unwrap();
        f.write_at(7, b"-lost").unwrap();
        f.write_at(0, b"TORN!!!").unwrap();
        disk.crash();
        let got = contents(&disk, "t.wav");
        assert_eq!(got.len(), 7, "seed {seed}");
        // Some prefix of the last write, then the durable remainder.
        let torn = got
            .iter()
            .zip(b"TORN!!!")
            .take_while(|(a, b)| a == b)
            .count();
        assert_eq!(&got[torn..], &b"durable"[torn..], "seed {seed}: {got:?}");
    }
}

#[test]
fn tears_vary_with_seed_and_repeat_with_the_same_seed() {
    let run = |seed| {
        let disk = SimDisk::new(seed);
        let mut f = disk.open("x").unwrap();
        f.write_at(0, &[0u8; 64]).unwrap();
        f.sync().unwrap();
        f.write_at(0, &[1u8; 64]).unwrap();
        disk.crash();
        contents(&disk, "x").iter().filter(|&&b| b == 1).count()
    };
    let outcomes: Vec<usize> = (0..20).map(run).collect();
    assert_eq!(outcomes, (0..20).map(run).collect::<Vec<_>>());
    assert!(outcomes.iter().any(|&n| n != outcomes[0]), "{outcomes:?}");
}

#[test]
fn never_synced_files_vanish_and_synced_set_len_survives() {
    let disk = SimDisk::new(3);
    disk.open("never").unwrap().write_at(0, b"x").unwrap();
    let mut f = disk.open("kept").unwrap();
    f.write_at(0, b"abcdef").unwrap();
    f.set_len(3).unwrap();
    f.sync().unwrap();
    f.set_len(1).unwrap();
    disk.crash();
    assert_eq!(disk.list().unwrap(), vec!["kept"]);
    assert_eq!(contents(&disk, "kept"), b"abc");
}

#[test]
fn handles_die_at_a_crash() {
    let disk = SimDisk::new(0);
    let mut f = disk.open("a").unwrap();
    f.sync().unwrap();
    disk.crash();
    assert!(f.write_at(0, b"zombie").is_err());
    assert!(f.len().is_err());
    assert!(disk.open("a").unwrap().is_empty().unwrap());
}
