//! Data for a host-encoded QR code. Payloads are never truncated into another code.
use super::*;

pub const MAX_QR_PAYLOAD_BYTES: usize = 8192;
pub const MAX_QR_CODES: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QrCorrection {
    Low,
    Medium,
    Quartile,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QrVersion {
    Normal(u8),
    Micro(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum QrSize {
    Cell(f32),
    Total(f32),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Qr {
    pub payload: Option<Vec<u8>>,
    pub correction: Option<QrCorrection>,
    pub version: Option<QrVersion>,
    pub size: Option<QrSize>,
    pub cell: Option<Rgba>,
    pub background: Option<Rgba>,
}

impl Qr {
    pub(super) fn sanitize(&mut self, bytes: &mut usize, codes: &mut usize) {
        let version_valid = match self.version {
            Some(QrVersion::Normal(value)) => (1..=40).contains(&value),
            Some(QrVersion::Micro(value)) => (1..=4).contains(&value),
            None => true,
        };
        if let Some(payload) = &self.payload {
            if !version_valid
                || payload.len() > MAX_QR_PAYLOAD_BYTES
                || payload.len() > *bytes
                || *codes == 0
            {
                self.payload = None;
            } else {
                *bytes -= payload.len();
                *codes -= 1;
            }
        }
        if let Some(size) = &mut self.size {
            match size {
                QrSize::Cell(value) => *value = bounded(*value).min(MAX_PIXELS / 182.0),
                QrSize::Total(value) => *value = bounded(*value).min(MAX_PIXELS / 3.0),
            }
        }
        bound_color(&mut self.cell);
        bound_color(&mut self.background);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn limits_drop_whole_payloads_and_bound_encoding_work() {
        let mut code = Qr {
            payload: Some(b"abc".to_vec()),
            size: Some(QrSize::Cell(f32::INFINITY)),
            ..Default::default()
        };
        let (mut bytes, mut codes) = (2, 1);
        code.sanitize(&mut bytes, &mut codes);
        assert_eq!(code.payload, None, "never encode a truncated payload");
        assert_eq!((bytes, codes), (2, 1));
        assert_eq!(code.size, Some(QrSize::Cell(MAX_PIXELS / 182.0)));
        code.payload = Some(vec![]);
        code.sanitize(&mut bytes, &mut codes);
        assert_eq!(code.payload, Some(vec![]));
        assert_eq!(codes, 0, "even empty codes spend encoding work");
        code.sanitize(&mut bytes, &mut codes);
        assert_eq!(code.payload, None);
        code.payload = Some(vec![0; MAX_QR_PAYLOAD_BYTES + 1]);
        let (mut bytes, mut codes) = (usize::MAX, 1);
        code.sanitize(&mut bytes, &mut codes);
        assert_eq!(code.payload, None);
        for version in [
            QrVersion::Normal(0),
            QrVersion::Normal(41),
            QrVersion::Micro(0),
            QrVersion::Micro(5),
        ] {
            code.payload = Some(b"data".to_vec());
            code.version = Some(version);
            code.sanitize(&mut bytes, &mut codes);
            assert_eq!(code.payload, None, "{version:?}");
        }
    }

    #[test]
    fn frame_limits_count_empty_codes_and_share_payload_bytes() {
        for (payload, count, expected) in [
            (vec![], MAX_QR_CODES + 1, MAX_QR_CODES),
            (
                vec![0; MAX_QR_PAYLOAD_BYTES],
                9,
                MAX_TEXT_BYTES_PER_FRAME / MAX_QR_PAYLOAD_BYTES,
            ),
        ] {
            let root = Node::Stack {
                key: "codes".into(),
                width: None,
                height: None,
                padding: None,
                background: None,
                border: None,
                clip: false,
                under: 0,
                children: (0..count)
                    .map(|id| Node::Qr {
                        key: id.to_string(),
                        code: Qr {
                            payload: Some(payload.clone()),
                            ..Default::default()
                        },
                    })
                    .collect(),
            };
            let mut frame: Frame = decode(&encode(&Frame {
                root: Some(root),
                ..Default::default()
            }))
            .unwrap();
            sanitize(&mut frame).unwrap();
            let kept = frame
                .root
                .as_ref()
                .unwrap()
                .children()
                .iter()
                .filter(|node| {
                    matches!(
                        node,
                        Node::Qr {
                            code: Qr {
                                payload: Some(_),
                                ..
                            },
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(kept, expected);
            let once = frame.clone();
            sanitize(&mut frame).unwrap();
            assert_eq!(once, frame);
        }
    }
}
