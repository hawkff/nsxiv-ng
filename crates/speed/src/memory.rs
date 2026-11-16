//! Peak memory of a process and its live descendants, from `/proc`.

use std::fs;

/// The VmHWM line of a `/proc/PID/status` text, in KiB.
pub fn vm_hwm_kib(status: &str) -> Option<u64> {
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?;
    value.trim().strip_suffix("kB")?.trim().parse().ok()
}

/// The parent's pid in a `/proc/PID/stat` text.
pub fn parent(stat: &str) -> Option<u32> {
    // the name in parentheses may hold spaces and parentheses, so fields count from the last one
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The sum of the KiB of `root` and of every descendant, out of (pid, parent, KiB) triples.
/// None when `root` is not among them.
pub fn tree_kib(root: u32, processes: &[(u32, u32, u64)]) -> Option<u64> {
    processes.iter().find(|p| p.0 == root)?;
    let mut tree = vec![root];
    let mut i = 0;
    while let Some(&pid) = tree.get(i) {
        for &(child, parent, _) in processes {
            if parent == pid && !tree.contains(&child) {
                tree.push(child);
            }
        }
        i += 1;
    }
    Some(
        processes
            .iter()
            .filter(|(pid, _, _)| tree.contains(pid))
            .map(|&(_, _, kib)| kib)
            .sum(),
    )
}

/// The VmHWM of `root` and its live descendants added up, in MiB. A descendant that ends while
/// it is read, or has no memory left (a zombie), counts nothing; such a root is an error.
pub fn peak_mib(root: u32) -> std::io::Result<f64> {
    let mut processes = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let Some(pid) = entry?.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        let read = |file: &str| fs::read_to_string(format!("/proc/{pid}/{file}")).ok();
        if let (Some(parent), Some(kib)) = (
            read("stat").as_deref().and_then(parent),
            read("status").as_deref().and_then(vm_hwm_kib),
        ) {
            processes.push((pid, parent, kib));
        }
    }
    let kib = tree_kib(root, &processes)
        .ok_or_else(|| std::io::Error::other(format!("no memory of process {root} to read")))?;
    Ok(kib as f64 / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "Name:\tnsxiv-ng\nUmask:\t0022\nState:\tS (sleeping)\n\
        VmPeak:\t 1301228 kB\nVmSize:\t 1235692 kB\nVmLck:\t       0 kB\nVmHWM:\t  245112 kB\n\
        VmRSS:\t  201044 kB\nThreads:\t5\n";

    #[test]
    fn vm_hwm_is_read_from_its_own_line() {
        assert_eq!(vm_hwm_kib(STATUS), Some(245_112));
        assert_eq!(vm_hwm_kib("Name:\tzombie\nState:\tZ (zombie)\n"), None);
        assert_eq!(vm_hwm_kib("VmHWM:\t  12 MB\n"), None);
    }

    #[test]
    fn the_parent_follows_a_name_with_spaces_and_parentheses() {
        assert_eq!(parent("4242 (nsxiv-ng) S 4200 4242 4200 0 -1"), Some(4200));
        assert_eq!(parent("77 (a b) c)) R 1 77 77 0 -1"), Some(1));
        assert_eq!(parent("77 (truncated"), None);
    }

    #[test]
    fn a_tree_adds_up_the_root_and_every_descendant() {
        let processes = [
            (1, 0, 10_000),
            (100, 1, 200),  // the viewer
            (101, 100, 30), // a helper
            (102, 100, 40), // another helper
            (103, 102, 5),  // the helper's child
            (104, 1, 999),  // the viewer's sibling
        ];
        assert_eq!(tree_kib(100, &processes), Some(275));
        assert_eq!(tree_kib(102, &processes), Some(45));
        assert_eq!(tree_kib(103, &processes), Some(5));
        // a root whose memory cannot be read gives no sum, never 0
        assert_eq!(tree_kib(55, &processes), None);
    }

    #[test]
    fn a_tree_ends_where_parents_loop() {
        assert_eq!(tree_kib(1, &[(1, 2, 3), (2, 1, 4)]), Some(7));
    }
}
