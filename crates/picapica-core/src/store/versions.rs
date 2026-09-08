use std::collections::BTreeSet;

/// why: 树视图同一列既要镜像 tag，也要软件包版本；digest 钉只服务引用计数，不能当版本号。
pub(super) fn versions_from_names(names: &[String]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for name in names {
        if let Some(tag) = human_tag(name) {
            out.insert(tag);
            continue;
        }
        if let Some(ver) = file_version(name) {
            out.insert(ver);
        }
    }
    out.into_iter().collect()
}

/// why: 界面展示的是 tag/版本号，删除时要映射回 refs.name，不能误伤 digest 钉。
pub(super) fn ref_names_for_version(names: &[String], version: &str) -> Vec<String> {
    names
        .iter()
        .filter(|name| {
            human_tag(name).as_deref() == Some(version)
                || file_version(name).as_deref() == Some(version)
        })
        .cloned()
        .collect()
}

fn human_tag(name: &str) -> Option<String> {
    let tag = name.strip_prefix("tag:")?;
    if tag.starts_with("sha256:") {
        return None;
    }
    Some(tag.to_string())
}

fn file_version(name: &str) -> Option<String> {
    let decoded = percent_decode(name)?;
    let base = decoded.rsplit('/').next().unwrap_or(decoded.as_str());
    deb_version(base).or_else(|| rpm_version(base))
}

/// why: 华为云 docker-ce 路径把 ~ 编成 %7e，不解码会把版本切错。
fn percent_decode(input: &str) -> Option<String> {
    if !input.contains('%') {
        return Some(input.to_string());
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_val(bytes[i + 1])?;
            let lo = hex_val(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn deb_version(base: &str) -> Option<String> {
    let stem = base
        .strip_suffix(".deb")
        .or_else(|| base.strip_suffix(".udeb"))
        .or_else(|| base.strip_suffix(".dsc"))?;
    let mut parts = stem.split('_');
    let pkg = parts.next()?;
    let ver = parts.next()?;
    if pkg.is_empty() || ver.is_empty() {
        return None;
    }
    Some(ver.to_string())
}

fn rpm_version(base: &str) -> Option<String> {
    let stem = base.strip_suffix(".rpm")?;
    let stem = strip_rpm_arch(stem);
    let mut segs: Vec<&str> = stem.split('-').collect();
    let idx = segs.iter().position(|s| starts_version(s))?;
    if idx == 0 {
        return None;
    }
    segs.drain(0..idx);
    let ver = segs.join("-");
    if ver.is_empty() {
        return None;
    }
    Some(ver)
}

fn strip_rpm_arch(stem: &str) -> &str {
    let Some((rest, arch)) = stem.rsplit_once('.') else {
        return stem;
    };
    match arch {
        "x86_64" | "aarch64" | "noarch" | "i386" | "i686" | "armv7hl" | "ppc64le" | "s390x"
        | "riscv64" | "src" => rest,
        _ => stem,
    }
}

fn starts_version(seg: &str) -> bool {
    seg.bytes().next().is_some_and(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_pins_are_not_versions() {
        let names = vec![
            "tag:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            "tag:16".into(),
            "tag:latest".into(),
        ];
        assert_eq!(versions_from_names(&names), vec!["16", "latest"]);
    }

    #[test]
    fn deb_and_rpm_filenames_yield_versions() {
        let names = vec![
            "dists/jammy/pool/stable/amd64/docker-ce_29.6.2-1%7eubuntu.22.04%7ejammy_amd64.deb"
                .into(),
            "pool/main/c/containerd.io/containerd.io_2.2.6-1~ubuntu.22.04~jammy_amd64.deb".into(),
            "Packages/d/docker-ce-26.1.4-1.fc40.x86_64.rpm".into(),
            "dists/jammy/InRelease".into(),
        ];
        assert_eq!(
            versions_from_names(&names),
            vec![
                "2.2.6-1~ubuntu.22.04~jammy",
                "26.1.4-1.fc40",
                "29.6.2-1~ubuntu.22.04~jammy",
            ]
        );
        assert_eq!(
            ref_names_for_version(&names, "29.6.2-1~ubuntu.22.04~jammy"),
            vec![names[0].clone()]
        );
    }

    #[test]
    fn version_delete_targets_human_tag_only() {
        let digest = "tag:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let names = vec![digest.into(), "tag:latest".into(), "tag:16".into()];
        assert_eq!(ref_names_for_version(&names, "latest"), vec!["tag:latest"]);
        assert!(ref_names_for_version(&names, digest.trim_start_matches("tag:")).is_empty());
    }
}
