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
        disk.sync_dir().unwrap();
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
        disk.sync_dir().unwrap();
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
fn unsynced_contents_vanish_and_synced_set_len_survives() {
    let disk = SimDisk::new(3);
    let mut f = disk.open("kept").unwrap();
    disk.sync_dir().unwrap();
    f.write_at(0, b"abcdef").unwrap();
    f.set_len(3).unwrap();
    f.sync().unwrap();
    f.set_len(1).unwrap();
    disk.open("never").unwrap().write_at(0, b"x").unwrap();
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

#[test]
fn creates_and_removes_need_sync_dir_to_survive() {
    let disk = SimDisk::new(5);
    let sub = disk.subdir("take01").unwrap();
    for (dir, name) in [(&disk, "a"), (&sub, "b")] {
        let mut f = dir.open(name).unwrap();
        f.write_at(0, b"synced data").unwrap();
        f.sync().unwrap();
    }
    // Synced contents, but no directory sync: both files vanish.
    disk.crash();
    assert!(disk.list().unwrap().is_empty());
    assert!(sub.list().unwrap().is_empty());

    let mut f = sub.open("b").unwrap();
    f.write_at(0, b"kept").unwrap();
    f.sync().unwrap();
    sub.sync_dir().unwrap();
    disk.open("never-dir-synced").unwrap().sync().unwrap();
    // A file whose entry is durable but whose contents never were synced comes back
    // empty, or with a torn prefix of its last write.
    sub.open("empty").unwrap().write_at(0, b"lost").unwrap();
    sub.sync_dir().unwrap();
    disk.crash();
    assert!(disk.list().unwrap().is_empty());
    assert_eq!(sub.list().unwrap(), vec!["b", "empty"]);
    assert_eq!(contents(&sub, "b"), b"kept");
    assert!(b"lost".starts_with(&contents(&sub, "empty")));

    // A remove without a directory sync is undone by a crash, contents and all.
    sub.remove("b").unwrap();
    disk.crash();
    assert_eq!(contents(&sub, "b"), b"kept");
    sub.remove("b").unwrap();
    sub.sync_dir().unwrap();
    disk.crash();
    assert_eq!(sub.list().unwrap(), vec!["empty"]);
}

#[test]
fn sync_dir_covers_only_its_own_directory() {
    let disk = SimDisk::new(6);
    let sub = disk.subdir("d").unwrap();
    disk.open("top").unwrap().sync().unwrap();
    sub.open("inner").unwrap().sync().unwrap();
    disk.sync_dir().unwrap();
    disk.crash();
    assert_eq!(disk.list().unwrap(), vec!["top"]);
    assert!(sub.list().unwrap().is_empty());
}
