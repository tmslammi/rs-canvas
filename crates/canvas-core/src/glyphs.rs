//! The shape library: original, MIT-licensed glyphs drawn in the app's
//! hand-drawn style.
//!
//! GCP product names are used nominatively to reference Google Cloud
//! services; the artwork is original and imitates no Google marks or brand
//! colours (see ADR-0004).

use crate::geometry::{Point, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlyphCategory {
    Compute,
    StorageData,
    Networking,
    DevOps,
    AiMl,
    Security,
    Observability,
    Generic,
}

impl GlyphCategory {
    pub fn key(self) -> &'static str {
        match self {
            GlyphCategory::Compute => "compute",
            GlyphCategory::StorageData => "storage",
            GlyphCategory::Networking => "networking",
            GlyphCategory::DevOps => "devops",
            GlyphCategory::AiMl => "ai",
            GlyphCategory::Security => "security",
            GlyphCategory::Observability => "observability",
            GlyphCategory::Generic => "generic",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GlyphCategory::Compute => "Compute",
            GlyphCategory::StorageData => "Storage & Data",
            GlyphCategory::Networking => "Networking",
            GlyphCategory::DevOps => "DevOps",
            GlyphCategory::AiMl => "AI / ML",
            GlyphCategory::Security => "Security",
            GlyphCategory::Observability => "Observability",
            GlyphCategory::Generic => "Generic",
        }
    }
}

/// One primitive in a glyph, in normalized 0..1 coordinates.
#[derive(Clone, Copy, Debug)]
pub enum GlyphShape {
    /// An open polyline.
    Polyline(&'static [(f64, f64)]),
    /// A closed rectangle.
    Rect(f64, f64, f64, f64),
    /// A closed circle.
    Circle(f64, f64, f64),
}

pub struct GlyphDef {
    pub id: &'static str,
    pub name: &'static str,
    pub category: GlyphCategory,
    pub shapes: &'static [GlyphShape],
}

/// A catalog entry for the panel UI.
#[derive(Clone, Debug, serde::Serialize)]
pub struct GlyphInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub category: &'static str,
}

use GlyphShape::{Circle, Polyline, Rect as GRect};

const GLOBE: &[GlyphShape] = &[
    Circle(0.5, 0.5, 0.42),
    Polyline(&[(0.08, 0.5), (0.92, 0.5)]),
    Polyline(&[
        (0.5, 0.08),
        (0.38, 0.28),
        (0.36, 0.5),
        (0.38, 0.72),
        (0.5, 0.92),
    ]),
    Polyline(&[
        (0.5, 0.08),
        (0.62, 0.28),
        (0.64, 0.5),
        (0.62, 0.72),
        (0.5, 0.92),
    ]),
];

const CYLINDER: &[GlyphShape] = &[
    Polyline(&[(0.18, 0.22), (0.18, 0.8)]),
    Polyline(&[(0.82, 0.22), (0.82, 0.8)]),
    Polyline(&[(0.18, 0.8), (0.32, 0.9), (0.68, 0.9), (0.82, 0.8)]),
    Polyline(&[
        (0.18, 0.22),
        (0.32, 0.12),
        (0.68, 0.12),
        (0.82, 0.22),
        (0.68, 0.32),
        (0.32, 0.32),
        (0.18, 0.22),
    ]),
];

pub static GLYPHS: &[GlyphDef] = &[
    // -- compute ----------------------------------------------------------
    GlyphDef {
        id: "compute_engine",
        name: "Compute Engine",
        category: GlyphCategory::Compute,
        shapes: &[
            GRect(0.22, 0.22, 0.78, 0.78),
            GRect(0.4, 0.4, 0.6, 0.6),
            Polyline(&[(0.34, 0.22), (0.34, 0.08)]),
            Polyline(&[(0.5, 0.22), (0.5, 0.08)]),
            Polyline(&[(0.66, 0.22), (0.66, 0.08)]),
            Polyline(&[(0.34, 0.92), (0.34, 0.78)]),
            Polyline(&[(0.5, 0.92), (0.5, 0.78)]),
            Polyline(&[(0.66, 0.92), (0.66, 0.78)]),
        ],
    },
    GlyphDef {
        id: "gke",
        name: "GKE",
        category: GlyphCategory::Compute,
        shapes: &[
            Polyline(&[
                (0.5, 0.08),
                (0.86, 0.29),
                (0.86, 0.71),
                (0.5, 0.92),
                (0.14, 0.71),
                (0.14, 0.29),
                (0.5, 0.08),
            ]),
            Circle(0.5, 0.5, 0.2),
        ],
    },
    GlyphDef {
        id: "cloud_run",
        name: "Cloud Run",
        category: GlyphCategory::Compute,
        shapes: &[
            GRect(0.12, 0.2, 0.88, 0.62),
            Polyline(&[(0.3, 0.72), (0.7, 0.72)]),
            Polyline(&[(0.28, 0.86), (0.56, 0.86)]),
            Polyline(&[(0.62, 0.86), (0.72, 0.86)]),
        ],
    },
    GlyphDef {
        id: "cloud_functions",
        name: "Cloud Functions",
        category: GlyphCategory::Compute,
        shapes: &[
            GRect(0.14, 0.14, 0.86, 0.86),
            Polyline(&[(0.56, 0.26), (0.42, 0.5), (0.56, 0.5), (0.44, 0.76)]),
        ],
    },
    // -- storage & data ---------------------------------------------------
    GlyphDef {
        id: "cloud_storage",
        name: "Cloud Storage",
        category: GlyphCategory::StorageData,
        shapes: CYLINDER,
    },
    GlyphDef {
        id: "cloud_sql",
        name: "Cloud SQL",
        category: GlyphCategory::StorageData,
        shapes: &[
            Circle(0.5, 0.24, 0.24),
            Polyline(&[(0.26, 0.24), (0.26, 0.8)]),
            Polyline(&[(0.74, 0.24), (0.74, 0.8)]),
            Polyline(&[(0.26, 0.8), (0.38, 0.9), (0.62, 0.9), (0.74, 0.8)]),
            Polyline(&[(0.36, 0.52), (0.64, 0.52)]),
        ],
    },
    GlyphDef {
        id: "firestore",
        name: "Firestore",
        category: GlyphCategory::StorageData,
        shapes: &[
            Polyline(&[
                (0.24, 0.12),
                (0.62, 0.12),
                (0.8, 0.3),
                (0.8, 0.88),
                (0.24, 0.88),
                (0.24, 0.12),
            ]),
            Polyline(&[(0.62, 0.12), (0.62, 0.3), (0.8, 0.3)]),
            Polyline(&[(0.36, 0.5), (0.68, 0.5)]),
            Polyline(&[(0.36, 0.68), (0.68, 0.68)]),
        ],
    },
    GlyphDef {
        id: "bigtable",
        name: "Bigtable",
        category: GlyphCategory::StorageData,
        shapes: &[
            GRect(0.12, 0.2, 0.88, 0.8),
            Polyline(&[(0.12, 0.4), (0.88, 0.4)]),
            Polyline(&[(0.12, 0.6), (0.88, 0.6)]),
            Polyline(&[(0.38, 0.2), (0.38, 0.8)]),
            Polyline(&[(0.62, 0.2), (0.62, 0.8)]),
        ],
    },
    GlyphDef {
        id: "spanner",
        name: "Spanner",
        category: GlyphCategory::StorageData,
        shapes: &[
            Circle(0.5, 0.5, 0.4),
            Polyline(&[(0.1, 0.5), (0.9, 0.5)]),
            Polyline(&[(0.5, 0.1), (0.5, 0.9)]),
        ],
    },
    GlyphDef {
        id: "bigquery",
        name: "BigQuery",
        category: GlyphCategory::StorageData,
        shapes: &[
            Polyline(&[(0.16, 0.78), (0.16, 0.54)]),
            Polyline(&[(0.34, 0.78), (0.34, 0.42)]),
            Polyline(&[(0.52, 0.78), (0.52, 0.58)]),
            Circle(0.66, 0.36, 0.22),
            Polyline(&[(0.82, 0.52), (0.92, 0.64)]),
        ],
    },
    GlyphDef {
        id: "pubsub",
        name: "Pub/Sub",
        category: GlyphCategory::StorageData,
        shapes: &[
            Polyline(&[(0.12, 0.34), (0.7, 0.34)]),
            Polyline(&[(0.58, 0.22), (0.7, 0.34), (0.58, 0.46)]),
            Polyline(&[(0.88, 0.68), (0.3, 0.68)]),
            Polyline(&[(0.42, 0.56), (0.3, 0.68), (0.42, 0.8)]),
        ],
    },
    // -- networking -------------------------------------------------------
    GlyphDef {
        id: "vpc",
        name: "VPC",
        category: GlyphCategory::Networking,
        shapes: &[
            Circle(0.5, 0.2, 0.13),
            Circle(0.2, 0.78, 0.13),
            Circle(0.8, 0.78, 0.13),
            Polyline(&[(0.4, 0.32), (0.28, 0.66)]),
            Polyline(&[(0.6, 0.32), (0.72, 0.66)]),
            Polyline(&[(0.33, 0.78), (0.67, 0.78)]),
        ],
    },
    GlyphDef {
        id: "load_balancing",
        name: "Load Balancing",
        category: GlyphCategory::Networking,
        shapes: &[
            GRect(0.06, 0.38, 0.24, 0.62),
            Circle(0.76, 0.2, 0.1),
            Circle(0.76, 0.5, 0.1),
            Circle(0.76, 0.8, 0.1),
            Polyline(&[(0.24, 0.44), (0.5, 0.2), (0.66, 0.2)]),
            Polyline(&[(0.24, 0.5), (0.5, 0.5), (0.66, 0.5)]),
            Polyline(&[(0.24, 0.56), (0.5, 0.8), (0.66, 0.8)]),
        ],
    },
    GlyphDef {
        id: "cdn",
        name: "Cloud CDN",
        category: GlyphCategory::Networking,
        shapes: &[
            Circle(0.5, 0.5, 0.36),
            Polyline(&[(0.14, 0.5), (0.86, 0.5)]),
            Polyline(&[(0.5, 0.14), (0.5, 0.86)]),
            Polyline(&[(0.24, 0.24), (0.18, 0.1)]),
            Polyline(&[(0.76, 0.24), (0.82, 0.1)]),
        ],
    },
    GlyphDef {
        id: "dns",
        name: "Cloud DNS",
        category: GlyphCategory::Networking,
        shapes: &[
            Circle(0.5, 0.44, 0.32),
            Polyline(&[(0.18, 0.44), (0.82, 0.44)]),
            Polyline(&[(0.62, 0.78), (0.9, 0.78)]),
            Polyline(&[(0.76, 0.64), (0.9, 0.78), (0.76, 0.92)]),
        ],
    },
    GlyphDef {
        id: "interconnect",
        name: "Cloud Interconnect",
        category: GlyphCategory::Networking,
        shapes: &[
            GRect(0.08, 0.3, 0.3, 0.7),
            GRect(0.7, 0.3, 0.92, 0.7),
            Polyline(&[(0.3, 0.42), (0.7, 0.42)]),
            Polyline(&[(0.3, 0.58), (0.7, 0.58)]),
        ],
    },
    // -- devops -----------------------------------------------------------
    GlyphDef {
        id: "cloud_build",
        name: "Cloud Build",
        category: GlyphCategory::DevOps,
        shapes: &[
            Circle(0.4, 0.4, 0.26),
            Circle(0.4, 0.4, 0.1),
            Polyline(&[(0.6, 0.6), (0.9, 0.9)]),
            Polyline(&[(0.78, 0.9), (0.9, 0.9), (0.9, 0.78)]),
        ],
    },
    GlyphDef {
        id: "artifact_registry",
        name: "Artifact Registry",
        category: GlyphCategory::DevOps,
        shapes: &[
            Polyline(&[
                (0.5, 0.12),
                (0.9, 0.32),
                (0.5, 0.52),
                (0.1, 0.32),
                (0.5, 0.12),
            ]),
            Polyline(&[
                (0.1, 0.32),
                (0.1, 0.68),
                (0.5, 0.88),
                (0.9, 0.68),
                (0.9, 0.32),
            ]),
            Polyline(&[(0.5, 0.52), (0.5, 0.88)]),
        ],
    },
    // -- ai ---------------------------------------------------------------
    GlyphDef {
        id: "vertex_ai",
        name: "Vertex AI",
        category: GlyphCategory::AiMl,
        shapes: &[
            Polyline(&[
                (0.5, 0.1),
                (0.58, 0.42),
                (0.9, 0.5),
                (0.58, 0.58),
                (0.5, 0.9),
                (0.42, 0.58),
                (0.1, 0.5),
                (0.42, 0.42),
                (0.5, 0.1),
            ]),
            Circle(0.82, 0.18, 0.08),
        ],
    },
    // -- security ---------------------------------------------------------
    GlyphDef {
        id: "iam",
        name: "IAM",
        category: GlyphCategory::Security,
        shapes: &[
            Circle(0.34, 0.28, 0.14),
            Polyline(&[
                (0.1, 0.7),
                (0.16, 0.5),
                (0.34, 0.44),
                (0.52, 0.5),
                (0.58, 0.7),
            ]),
            Polyline(&[
                (0.76, 0.3),
                (0.9, 0.46),
                (0.9, 0.64),
                (0.76, 0.82),
                (0.62, 0.64),
                (0.62, 0.46),
                (0.76, 0.3),
            ]),
        ],
    },
    GlyphDef {
        id: "kms",
        name: "Cloud KMS",
        category: GlyphCategory::Security,
        shapes: &[
            Circle(0.32, 0.4, 0.2),
            Polyline(&[(0.46, 0.53), (0.88, 0.86)]),
            Polyline(&[(0.72, 0.7), (0.62, 0.8)]),
            Polyline(&[(0.84, 0.82), (0.74, 0.92)]),
        ],
    },
    GlyphDef {
        id: "secret_manager",
        name: "Secret Manager",
        category: GlyphCategory::Security,
        shapes: &[
            GRect(0.2, 0.44, 0.8, 0.86),
            Polyline(&[
                (0.34, 0.44),
                (0.34, 0.28),
                (0.5, 0.14),
                (0.66, 0.28),
                (0.66, 0.44),
            ]),
            Circle(0.5, 0.62, 0.06),
            Polyline(&[(0.5, 0.68), (0.5, 0.78)]),
        ],
    },
    // -- observability ----------------------------------------------------
    GlyphDef {
        id: "monitoring",
        name: "Cloud Monitoring",
        category: GlyphCategory::Observability,
        shapes: &[
            GRect(0.1, 0.2, 0.9, 0.8),
            Polyline(&[
                (0.2, 0.56),
                (0.36, 0.56),
                (0.44, 0.34),
                (0.54, 0.72),
                (0.62, 0.5),
                (0.8, 0.5),
            ]),
        ],
    },
    GlyphDef {
        id: "logging",
        name: "Cloud Logging",
        category: GlyphCategory::Observability,
        shapes: &[
            Polyline(&[
                (0.24, 0.12),
                (0.64, 0.12),
                (0.8, 0.3),
                (0.8, 0.88),
                (0.24, 0.88),
                (0.24, 0.12),
            ]),
            Polyline(&[(0.64, 0.12), (0.64, 0.3), (0.8, 0.3)]),
            Polyline(&[(0.36, 0.46), (0.68, 0.46)]),
            Polyline(&[(0.36, 0.62), (0.68, 0.62)]),
            Polyline(&[(0.36, 0.76), (0.56, 0.76)]),
        ],
    },
    // -- generic ----------------------------------------------------------
    GlyphDef {
        id: "user",
        name: "User",
        category: GlyphCategory::Generic,
        shapes: &[
            Circle(0.5, 0.3, 0.16),
            Polyline(&[
                (0.18, 0.88),
                (0.26, 0.6),
                (0.5, 0.52),
                (0.74, 0.6),
                (0.82, 0.88),
            ]),
        ],
    },
    GlyphDef {
        id: "internet",
        name: "Internet",
        category: GlyphCategory::Generic,
        shapes: GLOBE,
    },
    GlyphDef {
        id: "on_prem",
        name: "On-prem",
        category: GlyphCategory::Generic,
        shapes: &[
            GRect(0.16, 0.28, 0.84, 0.88),
            Polyline(&[(0.16, 0.44), (0.84, 0.44)]),
            Polyline(&[(0.16, 0.6), (0.84, 0.6)]),
            Polyline(&[(0.16, 0.74), (0.84, 0.74)]),
            Polyline(&[(0.3, 0.36), (0.4, 0.36)]),
            Polyline(&[(0.3, 0.52), (0.4, 0.52)]),
            Polyline(&[(0.3, 0.68), (0.4, 0.68)]),
        ],
    },
    GlyphDef {
        id: "database",
        name: "Database",
        category: GlyphCategory::Generic,
        shapes: CYLINDER,
    },
    GlyphDef {
        id: "queue",
        name: "Queue",
        category: GlyphCategory::Generic,
        shapes: &[
            GRect(0.16, 0.16, 0.84, 0.34),
            GRect(0.16, 0.42, 0.84, 0.6),
            GRect(0.16, 0.68, 0.84, 0.86),
        ],
    },
    GlyphDef {
        id: "bucket",
        name: "Bucket",
        category: GlyphCategory::Generic,
        shapes: &[
            Polyline(&[
                (0.18, 0.28),
                (0.82, 0.28),
                (0.72, 0.9),
                (0.28, 0.9),
                (0.18, 0.28),
            ]),
            Polyline(&[(0.3, 0.28), (0.5, 0.14), (0.7, 0.28)]),
        ],
    },
    GlyphDef {
        id: "server",
        name: "Server",
        category: GlyphCategory::Generic,
        shapes: &[
            GRect(0.18, 0.14, 0.82, 0.86),
            Polyline(&[(0.18, 0.38), (0.82, 0.38)]),
            Polyline(&[(0.18, 0.62), (0.82, 0.62)]),
            Circle(0.68, 0.26, 0.045),
            Circle(0.68, 0.5, 0.045),
            Circle(0.68, 0.74, 0.045),
        ],
    },
    GlyphDef {
        id: "load_balancer",
        name: "Load Balancer",
        category: GlyphCategory::Generic,
        shapes: &[
            GRect(0.06, 0.38, 0.24, 0.62),
            Circle(0.76, 0.2, 0.1),
            Circle(0.76, 0.5, 0.1),
            Circle(0.76, 0.8, 0.1),
            Polyline(&[(0.24, 0.44), (0.5, 0.2), (0.66, 0.2)]),
            Polyline(&[(0.24, 0.5), (0.5, 0.5), (0.66, 0.5)]),
            Polyline(&[(0.24, 0.56), (0.5, 0.8), (0.66, 0.8)]),
        ],
    },
];

pub fn lookup(id: &str) -> Option<&'static GlyphDef> {
    GLYPHS.iter().find(|glyph| glyph.id == id)
}

pub fn catalog() -> Vec<GlyphInfo> {
    GLYPHS
        .iter()
        .map(|glyph| GlyphInfo {
            id: glyph.id,
            name: glyph.name,
            category: glyph.category.key(),
        })
        .collect()
}

/// Map a glyph shape's normalized coordinates into a rectangle.
pub fn shape_points(shape: &GlyphShape, rect: Rect) -> (Vec<Point>, bool) {
    let map = |x: f64, y: f64| Point::new(rect.x + x * rect.w, rect.y + y * rect.h);
    match shape {
        GlyphShape::Polyline(points) => (points.iter().map(|(x, y)| map(*x, *y)).collect(), false),
        GlyphShape::Rect(x1, y1, x2, y2) => (
            vec![map(*x1, *y1), map(*x2, *y1), map(*x2, *y2), map(*x1, *y2)],
            true,
        ),
        GlyphShape::Circle(cx, cy, r) => {
            let n = 32;
            let points = (0..n)
                .map(|i| {
                    let t = i as f64 / n as f64 * std::f64::consts::TAU;
                    map(cx + r * t.cos(), cy + r * t.sin())
                })
                .collect();
            (points, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_the_curated_set() {
        assert!(GLYPHS.len() >= 30, "got {}", GLYPHS.len());
        let gcp = GLYPHS
            .iter()
            .filter(|g| g.category != GlyphCategory::Generic)
            .count();
        let generic = GLYPHS.len() - gcp;
        assert_eq!(gcp, 24, "curated GCP services");
        assert_eq!(generic, 8, "generic shapes");
    }

    #[test]
    fn ids_are_unique_and_lookup_works() {
        let mut ids: Vec<&str> = GLYPHS.iter().map(|g| g.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), GLYPHS.len());
        assert_eq!(lookup("cloud_run").map(|g| g.name), Some("Cloud Run"));
        assert!(lookup("not_a_service").is_none());
    }

    #[test]
    fn every_glyph_stays_inside_its_normalized_box() {
        let rect = Rect::new(0.0, 0.0, 100.0, 100.0);
        for glyph in GLYPHS {
            assert!(!glyph.shapes.is_empty(), "{} has geometry", glyph.id);
            for shape in glyph.shapes {
                let (points, _) = shape_points(shape, rect);
                for point in points {
                    assert!(
                        (-0.5..=100.5).contains(&point.x) && (-0.5..=100.5).contains(&point.y),
                        "{} escapes its box at {point:?}",
                        glyph.id
                    );
                }
            }
        }
    }

    #[test]
    fn catalog_json_carries_panel_fields() {
        let catalog = catalog();
        assert_eq!(catalog.len(), GLYPHS.len());
        let json = serde_json::to_string(&catalog).unwrap();
        assert!(json.contains("\"name\":\"Cloud Run\""));
        assert!(json.contains("\"category\":\"networking\""));
    }
}
