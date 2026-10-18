//! A program declares its tables in a `ducktape.tables` custom section, borsh `Vec<Table>`; a
//! module without one declares none, and one that does not decode, or comes twice, does not load.
use abi::role::ownership::{TABLES_SECTION, Table, TableKind};
use runtime::{Fault, Limits, Runtime};

fn leb(mut n: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let low = (n & 0x7f) as u8;
        n >>= 7;
        match n {
            0 => {
                bytes.push(low);
                return bytes;
            }
            _ => bytes.push(low | 0x80),
        }
    }
}

fn with_section(mut module: Vec<u8>, name: &str, data: &[u8]) -> Vec<u8> {
    let body = [leb(name.len()), name.as_bytes().to_vec(), data.to_vec()].concat();
    module.push(0);
    module.extend(leb(body.len()));
    module.extend(body);
    module
}

fn empty() -> Vec<u8> {
    wat::parse_str("(module (memory (export \"memory\") 1))").unwrap()
}

#[test]
fn the_probe_declares_a_shared_table_and_an_owned_one() {
    let runtime = Runtime::new(Limits::default());
    let code = runtime.load(fixture_wasm::PROBE).unwrap();
    let declared = vec![
        Table {
            prefix: b"s/".to_vec(),
            kind: TableKind::Shared,
        },
        Table {
            prefix: b"o/".to_vec(),
            kind: TableKind::Owned("probe.item".into()),
        },
    ];
    assert_eq!(code.tables(), declared.as_slice());
    assert_eq!(abi::encode(&declared), fixture_probe::TABLES);
}

#[test]
fn a_module_without_the_section_declares_no_table() {
    let runtime = Runtime::new(Limits::default());
    let code = runtime.load(&empty()).unwrap();
    assert!(code.tables().is_empty());
    let other = with_section(empty(), "ducktape.schema", b"x");
    assert!(runtime.load(&other).unwrap().tables().is_empty());
}

#[test]
fn a_section_that_does_not_decode_or_comes_twice_does_not_load() {
    let runtime = Runtime::new(Limits::default());
    let garbled = with_section(empty(), TABLES_SECTION, &[0xff]);
    assert!(matches!(runtime.load(&garbled), Err(Fault::Load(_))));
    let tables = abi::encode(&Vec::<Table>::new());
    let once = with_section(empty(), TABLES_SECTION, &tables);
    assert!(runtime.load(&once).unwrap().tables().is_empty());
    let twice = with_section(once, TABLES_SECTION, &tables);
    let Err(Fault::Load(sentence)) = runtime.load(&twice) else {
        panic!("a second tables section loads");
    };
    assert_eq!(
        sentence,
        "the program declares its ducktape.tables section twice"
    );
}
