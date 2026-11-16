//! Mathematical invariants for FEA stress tensors, displacements, and strains.

/// Symmetric 3D Cauchy stress tensor components
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StressTensor {
    pub sxx: f32,
    pub syy: f32,
    pub szz: f32,
    pub sxy: f32,
    pub syz: f32,
    pub szx: f32,
}

impl StressTensor {
    pub fn new(sxx: f32, syy: f32, szz: f32, sxy: f32, syz: f32, szx: f32) -> Self {
        Self { sxx, syy, szz, sxy, syz, szx }
    }

    /// Calculate equivalent von Mises stress:
    /// sigma_v = sqrt( 0.5 * [ (sxx-syy)^2 + (syy-szz)^2 + (szz-sxx)^2 + 6*(sxy^2 + syz^2 + szx^2) ] )
    pub fn von_mises(&self) -> f32 {
        let diff_xy = self.sxx - self.syy;
        let diff_yz = self.syy - self.szz;
        let diff_zx = self.szz - self.sxx;
        let shear_sum = self.sxy * self.sxy + self.syz * self.syz + self.szx * self.szx;

        let val = 0.5 * (diff_xy * diff_xy + diff_yz * diff_yz + diff_zx * diff_zx + 6.0 * shear_sum);
        if val > 0.0 {
            val.sqrt()
        } else {
            0.0
        }
    }

    /// Calculate hydrostatic (mean) stress:
    /// sigma_m = (sxx + syy + szz) / 3
    pub fn hydrostatic(&self) -> f32 {
        (self.sxx + self.syy + self.szz) / 3.0
    }

    /// Calculate the 3 ordered principal stresses: (sigma_1 >= sigma_2 >= sigma_3)
    /// Solves the characteristic cubic equation analytically via Cardano-Viète method.
    pub fn principal_stresses(&self) -> (f32, f32, f32) {
        let i1 = self.sxx + self.syy + self.szz;
        let i2 = self.sxx * self.syy + self.syy * self.szz + self.szz * self.sxx
            - (self.sxy * self.sxy + self.syz * self.syz + self.szx * self.szx);
        let i3 = self.sxx * (self.syy * self.szz - self.syz * self.syz)
            - self.sxy * (self.sxy * self.szz - self.syz * self.szx)
            + self.szx * (self.sxy * self.syz - self.syy * self.szx);

        let p1 = i1 * i1 - 3.0 * i2;
        if p1 <= 1e-12 {
            // Hydrostatic state
            let mean = i1 / 3.0;
            return (mean, mean, mean);
        }

        let q = i1 / 3.0;
        let p = (p1 / 9.0).sqrt();
        let r = (2.0 * i1 * i1 * i1 - 9.0 * i1 * i2 + 27.0 * i3) / (54.0 * p * p * p);

        // Clamp r to [-1, 1] to avoid float precision issues with acos
        let r_clamped = r.clamp(-1.0, 1.0);
        let phi = (r_clamped.acos()) / 3.0;

        let pi = std::f32::consts::PI;
        let mut s1 = q + 2.0 * p * phi.cos();
        let mut s2 = q + 2.0 * p * (phi + 2.0 * pi / 3.0).cos();
        let mut s3 = q + 2.0 * p * (phi + 4.0 * pi / 3.0).cos();

        // Sort descending: s1 >= s2 >= s3
        if s1 < s2 { std::mem::swap(&mut s1, &mut s2); }
        if s2 < s3 { std::mem::swap(&mut s2, &mut s3); }
        if s1 < s2 { std::mem::swap(&mut s1, &mut s2); }

        (s1, s2, s3)
    }

    /// Maximum shear stress (Tresca criterion):
    /// tau_max = (sigma_1 - sigma_3) / 2
    pub fn max_shear(&self) -> f32 {
        let (s1, _, s3) = self.principal_stresses();
        (s1 - s3) * 0.5
    }
}

/// Calculate displacement Euclidean norm: sqrt(ux^2 + uy^2 + uz^2)
pub fn displacement_magnitude(ux: f32, uy: f32, uz: f32) -> f32 {
    (ux * ux + uy * uy + uz * uz).sqrt()
}

use crate::model::{ComponentData, Dataset, FieldType, FrdModel};
use rayon::prelude::*;
use std::collections::HashSet;

/// Extrema statistics for a stress dataset at a given step
#[derive(Debug, Clone, PartialEq)]
pub struct StressExtremaSummary {
    pub step_number: u32,
    pub increment_number: u32,
    pub value: f32, // Time point or Frequency
    pub max_von_mises: (f32, u32),       // (value, node_id)
    pub min_von_mises: (f32, u32),
    pub max_principal_s1: (f32, u32),
    pub min_principal_s3: (f32, u32),
    pub max_tresca: (f32, u32),
    pub max_hydrostatic: (f32, u32),
    pub node_count: usize,
}

/// Extrema statistics for a displacement dataset at a given step
#[derive(Debug, Clone, PartialEq)]
pub struct DispExtremaSummary {
    pub step_number: u32,
    pub increment_number: u32,
    pub value: f32,
    pub max_disp: (f32, u32),
    pub min_disp: (f32, u32),
    pub node_count: usize,
}

fn find_comp_idx(comps: &[ComponentData], names: &[&str]) -> Option<usize> {
    comps.iter().position(|c| {
        let clean = c.name.trim().to_uppercase();
        names.iter().any(|&n| clean == n.to_uppercase())
    })
}

/// Compute stress invariants (von Mises, Principal Stresses, Tresca) in parallel with Rayon.
pub fn compute_dataset_stress_invariants(
    ds: &Dataset,
    filter_nodes: Option<&HashSet<u32>>,
) -> Option<StressExtremaSummary> {
    let comps = &ds.components;
    let idx_xx = find_comp_idx(comps, &["SXX", "XX", "S11", "11"])?;
    let idx_yy = find_comp_idx(comps, &["SYY", "YY", "S22", "22"])?;
    let idx_zz = find_comp_idx(comps, &["SZZ", "ZZ", "S33", "33"])?;
    let idx_xy = find_comp_idx(comps, &["SXY", "XY", "S12", "12"])?;
    let idx_yz = find_comp_idx(comps, &["SYZ", "YZ", "S23", "23"])?;
    let idx_zx = find_comp_idx(comps, &["SZX", "ZX", "S31", "31", "S13", "13"])?;

    let xx_vals = &comps[idx_xx].values;
    let yy_vals = &comps[idx_yy].values;
    let zz_vals = &comps[idx_zz].values;
    let xy_vals = &comps[idx_xy].values;
    let yz_vals = &comps[idx_yz].values;
    let zx_vals = &comps[idx_zx].values;

    let candidate_nodes: Vec<u32> = if let Some(filter) = filter_nodes {
        xx_vals.keys().copied().filter(|id| filter.contains(id)).collect()
    } else {
        xx_vals.keys().copied().collect()
    };

    if candidate_nodes.is_empty() {
        return None;
    }

    // Parallel invariant computation
    let results: Vec<(u32, f32, (f32, f32, f32), f32, f32)> = candidate_nodes
        .par_iter()
        .filter_map(|&nid| {
            let sxx = *xx_vals.get(&nid)?;
            let syy = *yy_vals.get(&nid)?;
            let szz = *zz_vals.get(&nid)?;
            let sxy = *xy_vals.get(&nid)?;
            let syz = *yz_vals.get(&nid)?;
            let szx = *zx_vals.get(&nid)?;

            let tensor = StressTensor::new(sxx, syy, szz, sxy, syz, szx);
            let vm = tensor.von_mises();
            let prin = tensor.principal_stresses();
            let tresca = tensor.max_shear();
            let hydro = tensor.hydrostatic();
            Some((nid, vm, prin, tresca, hydro))
        })
        .collect();

    if results.is_empty() {
        return None;
    }

    let mut max_vm = (-f32::INFINITY, 0);
    let mut min_vm = (f32::INFINITY, 0);
    let mut max_s1 = (-f32::INFINITY, 0);
    let mut min_s3 = (f32::INFINITY, 0);
    let mut max_tresca = (-f32::INFINITY, 0);
    let mut max_hydro = (-f32::INFINITY, 0);

    for (nid, vm, (s1, _, s3), tresca, hydro) in results {
        if vm > max_vm.0 { max_vm = (vm, nid); }
        if vm < min_vm.0 { min_vm = (vm, nid); }
        if s1 > max_s1.0 { max_s1 = (s1, nid); }
        if s3 < min_s3.0 { min_s3 = (s3, nid); }
        if tresca > max_tresca.0 { max_tresca = (tresca, nid); }
        if hydro > max_hydro.0 { max_hydro = (hydro, nid); }
    }

    Some(StressExtremaSummary {
        step_number: ds.step_number,
        increment_number: 1,
        value: ds.value,
        max_von_mises: max_vm,
        min_von_mises: min_vm,
        max_principal_s1: max_s1,
        min_principal_s3: min_s3,
        max_tresca,
        max_hydrostatic: max_hydro,
        node_count: candidate_nodes.len(),
    })
}

/// Compute displacement magnitude extrema in parallel with Rayon.
pub fn compute_dataset_disp_invariants(
    ds: &Dataset,
    filter_nodes: Option<&HashSet<u32>>,
) -> Option<DispExtremaSummary> {
    let comps = &ds.components;

    // Check if ALL magnitude component is explicitly available and populated
    let all_idx = find_comp_idx(comps, &["ALL", "MAG", "TOTAL"])
        .filter(|&i| !comps[i].values.is_empty());
    let x_idx = find_comp_idx(comps, &["D1", "X", "DX", "U1"]);
    let y_idx = find_comp_idx(comps, &["D2", "Y", "DY", "U2"]);
    let z_idx = find_comp_idx(comps, &["D3", "Z", "DZ", "U3"]);

    let candidate_nodes: Vec<u32> = if let Some(all_i) = all_idx {
        if let Some(filter) = filter_nodes {
            comps[all_i].values.keys().copied().filter(|id| filter.contains(id)).collect()
        } else {
            comps[all_i].values.keys().copied().collect()
        }
    } else if let Some(xi) = x_idx {
        if let Some(filter) = filter_nodes {
            comps[xi].values.keys().copied().filter(|id| filter.contains(id)).collect()
        } else {
            comps[xi].values.keys().copied().collect()
        }
    } else {
        return None;
    };

    if candidate_nodes.is_empty() {
        return None;
    }

    let results: Vec<(u32, f32)> = if let Some(all_i) = all_idx.filter(|&i| !comps[i].values.is_empty()) {
        let vals = &comps[all_i].values;
        candidate_nodes
            .par_iter()
            .filter_map(|&nid| vals.get(&nid).map(|&v| (nid, v)))
            .collect()
    } else {
        let xi = x_idx.unwrap();
        let yi = y_idx.unwrap();
        let zi = z_idx.unwrap();
        let x_vals = &comps[xi].values;
        let y_vals = &comps[yi].values;
        let z_vals = &comps[zi].values;

        candidate_nodes
            .par_iter()
            .filter_map(|&nid| {
                let ux = *x_vals.get(&nid)?;
                let uy = *y_vals.get(&nid)?;
                let uz = *z_vals.get(&nid)?;
                Some((nid, displacement_magnitude(ux, uy, uz)))
            })
            .collect()
    };

    if results.is_empty() {
        return None;
    }

    let mut max_disp = (-f32::INFINITY, 0);
    let mut min_disp = (f32::INFINITY, 0);

    for (nid, d) in results {
        if d > max_disp.0 { max_disp = (d, nid); }
        if d < min_disp.0 { min_disp = (d, nid); }
    }

    Some(DispExtremaSummary {
        step_number: ds.step_number,
        increment_number: 1,
        value: ds.value,
        max_disp,
        min_disp,
        node_count: candidate_nodes.len(),
    })
}

impl FrdModel {
    /// Compute peak stress history across all steps/increments in the model
    pub fn compute_stress_history(
        &self,
        filter_nodes: Option<&HashSet<u32>>,
    ) -> Vec<StressExtremaSummary> {
        let mut history = Vec::new();
        for step in &self.steps {
            for ds in &step.datasets {
                if ds.field_type == FieldType::Stress || ds.name.to_uppercase().contains("STRESS") {
                    if let Some(mut summ) = compute_dataset_stress_invariants(ds, filter_nodes) {
                        summ.step_number = step.step_number;
                        summ.increment_number = step.increment_number;
                        summ.value = step.value;
                        history.push(summ);
                    }
                }
            }
        }
        history
    }

    /// Compute peak displacement history across all steps/increments in the model
    pub fn compute_disp_history(
        &self,
        filter_nodes: Option<&HashSet<u32>>,
    ) -> Vec<DispExtremaSummary> {
        let mut history = Vec::new();
        for step in &self.steps {
            for ds in &step.datasets {
                if ds.field_type == FieldType::Displacement || ds.name.to_uppercase().contains("DISP") {
                    if let Some(mut summ) = compute_dataset_disp_invariants(ds, filter_nodes) {
                        summ.step_number = step.step_number;
                        summ.increment_number = step.increment_number;
                        summ.value = step.value;
                        history.push(summ);
                    }
                }
            }
        }
        history
    }
}

impl crate::parser::DenseDataset {
    /// Stress invariant extrema over the dataset, optionally restricted to `filter_nodes`.
    /// Same quantities as `compute_dataset_stress_invariants`. Ties keep the first node in file order.
    pub fn stress_extrema(&self, filter_nodes: Option<&HashSet<u32>>) -> Option<StressExtremaSummary> {
        let n = self.node_ids.len();
        let full = |names: &[&str]| self.column(names).filter(|c| c.len() == n);
        let xx = full(&["SXX", "XX", "S11", "11"])?;
        let yy = full(&["SYY", "YY", "S22", "22"])?;
        let zz = full(&["SZZ", "ZZ", "S33", "33"])?;
        let xy = full(&["SXY", "XY", "S12", "12"])?;
        let yz = full(&["SYZ", "YZ", "S23", "23"])?;
        let zx = full(&["SZX", "ZX", "S31", "31", "S13", "13"])?;

        let mut max_vm = (-f32::INFINITY, 0);
        let mut min_vm = (f32::INFINITY, 0);
        let mut max_s1 = (-f32::INFINITY, 0);
        let mut min_s3 = (f32::INFINITY, 0);
        let mut max_tresca = (-f32::INFINITY, 0);
        let mut max_hydro = (-f32::INFINITY, 0);
        let mut node_count = 0usize;

        for (i, &nid) in self.node_ids.iter().enumerate() {
            if filter_nodes.is_some_and(|f| !f.contains(&nid)) {
                continue;
            }
            node_count += 1;
            let tensor = StressTensor::new(xx[i], yy[i], zz[i], xy[i], yz[i], zx[i]);
            let vm = tensor.von_mises();
            let (s1, _, s3) = tensor.principal_stresses();
            let tresca = (s1 - s3) * 0.5;
            let hydro = tensor.hydrostatic();
            if vm > max_vm.0 { max_vm = (vm, nid); }
            if vm < min_vm.0 { min_vm = (vm, nid); }
            if s1 > max_s1.0 { max_s1 = (s1, nid); }
            if s3 < min_s3.0 { min_s3 = (s3, nid); }
            if tresca > max_tresca.0 { max_tresca = (tresca, nid); }
            if hydro > max_hydro.0 { max_hydro = (hydro, nid); }
        }

        if node_count == 0 {
            return None;
        }
        Some(StressExtremaSummary {
            step_number: self.step_number,
            increment_number: 1,
            value: self.value,
            max_von_mises: max_vm,
            min_von_mises: min_vm,
            max_principal_s1: max_s1,
            min_principal_s3: min_s3,
            max_tresca,
            max_hydrostatic: max_hydro,
            node_count,
        })
    }

    /// Displacement magnitude extrema, optionally restricted to `filter_nodes`.
    /// Same quantities as `compute_dataset_disp_invariants`.
    pub fn disp_extrema(&self, filter_nodes: Option<&HashSet<u32>>) -> Option<DispExtremaSummary> {
        let n = self.node_ids.len();
        let full = |names: &[&str]| self.column(names).filter(|c| c.len() == n && n > 0);
        let all = full(&["ALL", "MAG", "TOTAL"]);
        let xyz = match all {
            Some(_) => None,
            None => Some((
                full(&["D1", "X", "DX", "U1"])?,
                full(&["D2", "Y", "DY", "U2"])?,
                full(&["D3", "Z", "DZ", "U3"])?,
            )),
        };

        let mut max_disp = (-f32::INFINITY, 0);
        let mut min_disp = (f32::INFINITY, 0);
        let mut node_count = 0usize;

        for (i, &nid) in self.node_ids.iter().enumerate() {
            if filter_nodes.is_some_and(|f| !f.contains(&nid)) {
                continue;
            }
            node_count += 1;
            let d = match (all, xyz) {
                (Some(a), _) => a[i],
                (None, Some((x, y, z))) => displacement_magnitude(x[i], y[i], z[i]),
                (None, None) => return None,
            };
            if d > max_disp.0 { max_disp = (d, nid); }
            if d < min_disp.0 { min_disp = (d, nid); }
        }

        if node_count == 0 {
            return None;
        }
        Some(DispExtremaSummary {
            step_number: self.step_number,
            increment_number: 1,
            value: self.value,
            max_disp,
            min_disp,
            node_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uniaxial_von_mises() {
        let s = StressTensor::new(100.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        assert!((s.von_mises() - 100.0).abs() < 1e-4);
        let (s1, s2, s3) = s.principal_stresses();
        assert!((s1 - 100.0).abs() < 1e-4);
        assert!(s2.abs() < 1e-4);
        assert!(s3.abs() < 1e-4);
        assert!((s.max_shear() - 50.0).abs() < 1e-4);
    }

    #[test]
    fn test_pure_shear_von_mises() {
        let s = StressTensor::new(0.0, 0.0, 0.0, 50.0, 0.0, 0.0);
        // Pure shear von Mises is sqrt(3) * tau
        let expected = (3.0_f32).sqrt() * 50.0;
        assert!((s.von_mises() - expected).abs() < 1e-4);
    }

    #[test]
    fn test_displacement_magnitude() {
        assert!((displacement_magnitude(3.0, 4.0, 0.0) - 5.0).abs() < 1e-5);
    }
}
