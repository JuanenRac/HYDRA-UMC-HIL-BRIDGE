// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/limits.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! The joint limits of the robot, read from its URDF.
//!
//! The bridge only knew the shape of a command (a name and a finite number). With `--urdf FILE` it also knows the joints of the robot and
//! the range each one may move in, and a real command for a joint the URDF does not have, or for a position outside its range, is blocked
//! before it is forwarded. The same rules as HYDRA-UMC-PHYSICS-REPLICA: a `revolute` or `prismatic` joint has the `lower`/`upper` of its
//! `<limit>`, a `continuous` joint turns without a position limit, a `fixed` joint does not move, and a `<limit>` that is missing or
//! inverted is an error when the file is read, never a silent "no limit".

use std::collections::HashMap;

/// The range of one joint.
#[derive(Debug, Clone, PartialEq)]
pub enum JointRange {
    /// `revolute` or `prismatic`: the position must be inside [lower, upper].
    Bounded { lower: f64, upper: f64 },
    /// `continuous`: any finite position is allowed.
    Unbounded,
    /// `fixed`: no command may move it.
    Fixed,
}

/// The joints of a robot and their ranges.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JointLimits {
    joints: HashMap<String, JointRange>,
}

impl JointLimits {
    pub fn len(&self) -> usize {
        self.joints.len()
    }

    /// Reads the URDF text. Joints that have a `<mimic>` follow another joint and cannot be commanded on their own: they are kept as
    /// `Fixed` for this purpose.
    pub fn from_urdf(xml: &str) -> Result<Self, String> {
        let document =
            roxmltree::Document::parse(xml).map_err(|e| format!("not valid XML: {e}"))?;
        let root = document.root_element();
        if root.tag_name().name() != "robot" {
            return Err("the root element is not <robot>".to_string());
        }
        let mut joints = HashMap::new();
        for joint in root.children().filter(|n| n.has_tag_name("joint")) {
            let name = joint
                .attribute("name")
                .filter(|n| !n.trim().is_empty())
                .ok_or("a <joint> has no name")?;
            let kind = joint.attribute("type").unwrap_or("");
            if joints.contains_key(name) {
                return Err(format!("joint {name:?} is declared twice"));
            }
            let mimics = joint.children().any(|n| n.has_tag_name("mimic"));
            let range = match kind {
                _ if mimics => JointRange::Fixed,
                "fixed" => JointRange::Fixed,
                "continuous" => JointRange::Unbounded,
                "revolute" | "prismatic" => {
                    let limit = joint
                        .children()
                        .find(|n| n.has_tag_name("limit"))
                        .ok_or_else(|| format!("{kind} joint {name:?} has no <limit>"))?;
                    let read = |attribute: &str| -> Result<f64, String> {
                        let text = limit.attribute(attribute).ok_or_else(|| {
                            format!("<limit> of joint {name:?} has no {attribute:?}")
                        })?;
                        let value: f64 = text.trim().parse().map_err(|_| {
                            format!(
                                "<limit> of joint {name:?}: {attribute}={text:?} is not a number"
                            )
                        })?;
                        if value.is_finite() {
                            Ok(value)
                        } else {
                            Err(format!(
                                "<limit> of joint {name:?}: {attribute} is not finite"
                            ))
                        }
                    };
                    let (lower, upper) = (read("lower")?, read("upper")?);
                    if lower > upper {
                        return Err(format!(
                            "<limit> of joint {name:?}: lower {lower} is above upper {upper}"
                        ));
                    }
                    JointRange::Bounded { lower, upper }
                }
                other => {
                    return Err(format!(
                        "joint {name:?} has the type {other:?}, which this bridge does not command"
                    ))
                }
            };
            joints.insert(name.to_string(), range);
        }
        if joints.is_empty() {
            return Err("the URDF has no <joint>".to_string());
        }
        Ok(Self { joints })
    }

    /// Whether a real command for this joint at this position may be forwarded. The error says why not.
    pub fn check(&self, joint: &str, position: f64) -> Result<(), String> {
        match self.joints.get(joint) {
            None => Err(format!("the URDF has no joint {joint:?}")),
            Some(JointRange::Fixed) => Err(format!(
                "joint {joint:?} is fixed (or follows another joint) and cannot be commanded"
            )),
            Some(JointRange::Unbounded) => Ok(()),
            Some(JointRange::Bounded { lower, upper }) => {
                if position < *lower || position > *upper {
                    Err(format!(
                        "position {position} of joint {joint:?} is outside its limits [{lower}, {upper}]"
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URDF: &str = r#"<robot name="arm">
        <link name="base"/><link name="l1"/><link name="l2"/><link name="l3"/>
        <joint name="shoulder" type="revolute"><parent link="base"/><child link="l1"/>
            <limit lower="-1.5" upper="2.0" effort="10" velocity="1"/></joint>
        <joint name="wrist" type="continuous"><parent link="l1"/><child link="l2"/></joint>
        <joint name="mount" type="fixed"><parent link="l2"/><child link="l3"/></joint>
        <joint name="finger" type="prismatic"><parent link="l2"/><child link="l3"/>
            <limit lower="0" upper="0.04" effort="5" velocity="0.1"/><mimic joint="shoulder"/></joint>
    </robot>"#;

    #[test]
    fn a_position_inside_the_limits_passes_and_the_edges_count_as_inside() {
        let limits = JointLimits::from_urdf(URDF).unwrap();
        assert_eq!(limits.len(), 4);
        for position in [-1.5, 0.0, 2.0] {
            assert!(limits.check("shoulder", position).is_ok(), "{position}");
        }
    }

    #[test]
    fn a_position_outside_the_limits_is_refused_with_the_range_in_the_reason() {
        let limits = JointLimits::from_urdf(URDF).unwrap();
        let reason = limits.check("shoulder", 2.0001).unwrap_err();
        assert!(reason.contains("[-1.5, 2]"), "{reason}");
        assert!(limits.check("shoulder", -1.6).is_err());
    }

    #[test]
    fn a_continuous_joint_has_no_position_limit() {
        let limits = JointLimits::from_urdf(URDF).unwrap();
        assert!(limits.check("wrist", 1.0e6).is_ok());
    }

    #[test]
    fn a_joint_the_urdf_does_not_have_is_refused() {
        let limits = JointLimits::from_urdf(URDF).unwrap();
        assert!(limits.check("elbow", 0.0).unwrap_err().contains("no joint"));
    }

    #[test]
    fn a_fixed_joint_and_a_joint_that_follows_another_are_not_commandable() {
        let limits = JointLimits::from_urdf(URDF).unwrap();
        assert!(limits.check("mount", 0.0).is_err());
        assert!(limits.check("finger", 0.01).is_err());
    }

    #[test]
    fn a_urdf_that_cannot_give_a_range_is_an_error_not_a_missing_limit() {
        let cases = [
            ("not xml", "not valid XML"),
            ("<other/>", "not <robot>"),
            ("<robot/>", "no <joint>"),
            (
                r#"<robot><joint name="a" type="revolute"/></robot>"#,
                "no <limit>",
            ),
            (
                r#"<robot><joint name="a" type="revolute"><limit upper="1"/></joint></robot>"#,
                "no \"lower\"",
            ),
            (
                r#"<robot><joint name="a" type="revolute"><limit lower="2" upper="1"/></joint></robot>"#,
                "above upper",
            ),
            (
                r#"<robot><joint name="a" type="revolute"><limit lower="x" upper="1"/></joint></robot>"#,
                "not a number",
            ),
            (
                r#"<robot><joint name="a" type="revolute"><limit lower="0" upper="inf"/></joint></robot>"#,
                "not finite",
            ),
            (
                r#"<robot><joint name="a" type="floating"/></robot>"#,
                "does not command",
            ),
            (
                r#"<robot><joint name="a" type="fixed"/><joint name="a" type="fixed"/></robot>"#,
                "twice",
            ),
        ];
        for (xml, expected) in cases {
            let error = JointLimits::from_urdf(xml).unwrap_err();
            assert!(error.contains(expected), "{xml}: {error}");
        }
    }
}
