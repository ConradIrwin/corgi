#![deny(unexpected_cfgs)]

#[cfg(test)]
fn test_cfg_is_expected() {}

#[cfg(docsrs)]
fn docsrs_cfg_is_expected() {}

#[cfg(feature = "declared")]
fn declared_feature_is_expected() {}

#[cfg(build_script_cfg)]
fn build_script_cfg_is_expected() {}

fn main() {}
