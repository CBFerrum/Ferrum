//! Fixtures shared by the unit tests: valid apps, hand-made archives and keys.

use crate::Signer;
use flate2::Compression;
use flate2::write::GzEncoder;
use std::collections::BTreeMap;
use std::io::Write;

pub(crate) const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

pub(crate) fn files(list: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
    list.iter().map(|(p, d)| (p.to_string(), d.to_vec())).collect()
}

/// A meta app: one entity, one role, a field on another package's entity.
pub(crate) fn meta_app() -> BTreeMap<String, Vec<u8>> {
    files(&[
        (
            "app.yaml",
            b"id: kit\nname: Kit\nversion: 1.0.0\npublisher: acme\nsummary: A starter kit\ndescription: README.md\ncategories: [industry]\n\
icon: assets/icon.svg\nruntime: meta\nplatform: \">=0.3, <0.4\"\npackages: [kit]\nprefix: Kt\n",
        ),
        ("README.md", b"# Kit\n"),
        ("LICENSE", b"MIT\n"),
        ("assets/icon.svg", b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
        (
            "packages/kit/package.yaml",
            b"package: kit\nversion: 1.0.0\nfield_prefix: kt_\nroles:\n  - name: Kit User\n    permissions:\n      - {entity: KtThing, read: true, write: true, create: true}\n\
overlays:\n  - entity: Customer\n    ops:\n      - {op: add_field, field: {name: kt_level, type: data}}\n",
        ),
        ("packages/kit/entities/thing.yaml", b"name: KtThing\nlabel: Thing\nkind: master\nfields:\n  - {name: title, type: data, required: true}\n"),
    ])
}

pub(crate) const GW_PLUGIN: &str = r#"{
  "name": "gw",
  "version": "1.0.0",
  "description": "Device gateway",
  "config": {"greeting": "hello"},
  "hooks": [{"entity": "GwDevice", "stage": "validate"}],
  "routes": [{"path": "/status", "public": true}, {"path": "/admin"}],
  "sockets": [{"path": "/devices/*", "public": true, "subprotocols": ["gw.v1"]}],
  "subscriptions": ["SalesInvoice.*"],
  "schedules": [{"name": "sweep", "every_seconds": 300}],
  "permissions": [{"entity": "GwDevice", "read": true, "write": true}, {"entity": "Customer", "read": true}],
  "http_hosts": ["api.example.com"],
  "entities": [{"name": "XGwReading", "label": "Reading", "kind": "master", "fields": [{"name": "value", "type": "decimal"}]}]
}"#;

/// A wasm app with a plugin that uses every kind of permission, and a package with SQL.
pub(crate) fn wasm_app() -> BTreeMap<String, Vec<u8>> {
    files(&[
        (
            "app.yaml",
            b"id: gw\nname: Gateway\nversion: 2.1.0\npublisher: acme\nsummary: Devices over WebSocket\ncategories: [iot]\nruntime: wasm\n\
platform: \"^0.3\"\nrequires: {kit: \"^1\"}\npackages: [gw]\nplugin: plugin.json\nprefix: Gw\nsettings:\n  - {key: api_key, label: API key, type: secret}\n",
        ),
        ("plugin.json", GW_PLUGIN.as_bytes()),
        ("plugin.wasm", b"\0asm\x01\0\0\0"),
        ("packages/gw/package.yaml", b"package: gw\nversion: 2.1.0\n"),
        ("packages/gw/entities/device.yaml", b"name: GwDevice\nlabel: Device\nkind: master\nfields:\n  - {name: serial, type: data}\n"),
        ("packages/gw/sql/001_init.sql", b"create index if not exists gw_serial on e_gw_device (serial);\n"),
    ])
}

/// One hand-made tar entry (names and types a well-behaved writer would never produce).
#[derive(Clone)]
pub(crate) struct RawEntry<'a> {
    pub name: &'a [u8],
    pub kind: tar::EntryType,
    pub data: &'a [u8],
    pub mtime: u64,
    pub mode: u32,
    pub link: Option<&'a str>,
}

impl<'a> RawEntry<'a> {
    pub(crate) fn file(name: &'a str, data: &'a [u8]) -> RawEntry<'a> {
        RawEntry { name: name.as_bytes(), kind: tar::EntryType::Regular, data, mtime: 0, mode: 0o644, link: None }
    }
}

/// A tar with the entries exactly as given (names up to 100 bytes, written verbatim).
pub(crate) fn raw_tar(entries: &[RawEntry]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for e in entries {
        let mut h = tar::Header::new_gnu();
        h.as_gnu_mut().unwrap().name[..e.name.len()].copy_from_slice(e.name);
        h.set_entry_type(e.kind);
        h.set_size(e.data.len() as u64);
        h.set_mode(e.mode);
        h.set_mtime(e.mtime);
        h.set_uid(1000);
        h.set_gid(1000);
        if let Some(link) = e.link {
            h.set_link_name(link).unwrap();
        }
        h.set_cksum();
        b.append(&h, e.data).unwrap();
    }
    b.into_inner().unwrap()
}

pub(crate) fn gz(data: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), Compression::fast());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

pub(crate) fn signer() -> Signer {
    Signer::from_pkcs8(&Signer::generate_pkcs8().unwrap()).unwrap()
}
