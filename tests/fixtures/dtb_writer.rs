use std::{fs, path::PathBuf, process::Command};
use writer::{with_modules, Error};

fn directory() -> PathBuf {
    std::env::var_os("TAU_DTB_TEST_DIR").unwrap().into()
}
fn fixture(name: &str, body: &str) -> Vec<u8> {
    let dts = directory().join(format!("{name}.dts"));
    let dtb = directory().join(format!("{name}.dtb"));
    fs::write(
        &dts,
        format!("/dts-v1/; /memreserve/ 0x12340000 0x1000; / {{ {body} }};"),
    )
    .unwrap();
    let output = Command::new("dtc")
        .args(["-I", "dts", "-O", "dtb", "-o"])
        .arg(&dtb)
        .arg(dts)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read(dtb).unwrap()
}
fn property(name: &str, bytes: &[u8], node: &str, property: &str, kind: &str) -> String {
    let path = directory().join(format!("{name}-output.dtb"));
    fs::write(&path, bytes).unwrap();
    let output = Command::new("fdtget")
        .args(["-t", kind])
        .arg(path)
        .args([node, property])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
fn write(source: &[u8], out: &mut [u8]) -> Result<usize, Error> {
    let module = layout::StaticModule {
        name: *b"dummy-test\0\0\0\0\0\0\0\0\0\0",
        offset: 0x57,
        pages: 8,
        user: true,
    };
    with_modules(source, out, 0x1_80200000, [&module].into_iter())
}

#[test]
fn preserves_chosen_properties_children_and_reservations() {
    let source = fixture(
        "chosen",
        "compatible = \"tau,test\"; chosen { bootargs = \"keep me\"; other { value = <42>; }; };",
    );
    let original = source.clone();
    let mut out = vec![0; 4096];
    let len = write(&source, &mut out).unwrap();
    assert_eq!(source, original);
    assert_eq!(
        property("chosen", &out[..len], "/chosen", "bootargs", "s"),
        "keep me"
    );
    assert_eq!(
        property("chosen", &out[..len], "/chosen/other", "value", "x"),
        "2a"
    );
    assert_eq!(
        property(
            "chosen",
            &out[..len],
            "/chosen/tau,modules/module@180257000",
            "tau,name",
            "s"
        ),
        "dummy-test"
    );
    assert_eq!(
        property(
            "chosen",
            &out[..len],
            "/chosen/tau,modules/module@180257000",
            "reg",
            "x"
        ),
        "1 80257000 0 8000"
    );
    let old = u32::from_be_bytes(source[16..20].try_into().unwrap()) as usize;
    assert_eq!(&out[40..72], &source[old..old + 32]);
}

#[test]
fn creates_chosen_and_supports_multiple_modules() {
    let source = fixture("no-chosen", "compatible = \"tau,test\";");
    let mut out = vec![0; 4096];
    let modules = [
        layout::StaticModule {
            name: *b"system\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
            offset: 16,
            pages: 16,
            user: true,
        },
        layout::StaticModule {
            name: *b"dummy-test\0\0\0\0\0\0\0\0\0\0",
            offset: 32,
            pages: 8,
            user: true,
        },
    ];
    let len = with_modules(&source, &mut out, 0x80200000, modules.iter()).unwrap();
    assert_eq!(
        property(
            "no-chosen",
            &out[..len],
            "/chosen/tau,modules",
            "#address-cells",
            "x"
        ),
        "2"
    );
    assert_eq!(
        property(
            "no-chosen",
            &out[..len],
            "/chosen/tau,modules/module@80210000",
            "tau,name",
            "s"
        ),
        "system"
    );
    assert_eq!(
        property(
            "no-chosen",
            &out[..len],
            "/chosen/tau,modules/module@80220000",
            "tau,name",
            "s"
        ),
        "dummy-test"
    );
}

#[test]
fn rejects_insufficient_capacity_and_truncated_input() {
    let source = fixture("invalid", "chosen {};");
    assert_eq!(write(&source, &mut [0; 40]), Err(Error::NoSpace));
    assert_eq!(
        write(&source[..source.len() - 1], &mut [0; 4096]),
        Err(Error::Invalid)
    );
    let mut malformed = source.clone();
    malformed[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(write(&malformed, &mut [0; 4096]), Err(Error::Invalid));
}

#[test]
fn refuses_ambiguous_existing_module_data() {
    let source = fixture("existing", "chosen { tau,modules {}; };");
    assert_eq!(write(&source, &mut [0; 4096]), Err(Error::ExistingModules));
}

#[test]
fn both_board_trees_fit_the_reserved_buffer() {
    let root = PathBuf::from(std::env::var_os("TAU_TEST_PROJECT").unwrap());
    for (name, path) in [
        ("qemu", "0-M/boards/qemu/virt.dtb"),
        (
            "vf2",
            "0-M/boards/vf2/jh7110-starfive-visionfive-2-v1.3b.dtb",
        ),
    ] {
        let source = fs::read(root.join(path)).unwrap();
        let mut out = vec![0; 65536];
        let len = write(&source, &mut out).unwrap();
        assert_eq!(
            property(
                name,
                &out[..len],
                "/chosen/tau,modules/module@180257000",
                "tau,name",
                "s"
            ),
            "dummy-test"
        );
        // The independent compiler validates the complete resulting tree.
        let input = directory().join(format!("{name}-output.dtb"));
        let output = Command::new("dtc")
            .args(["-I", "dtb", "-O", "dtb", "-o"])
            .arg(directory().join(format!("{name}-checked.dtb")))
            .arg(input)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
