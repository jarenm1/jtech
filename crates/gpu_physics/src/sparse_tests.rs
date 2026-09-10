use super::*;
use crate::sparse::{CHUNK_CELLS, PAGE_WORDS, TABLE_WORDS};
use crate::tests::GPU_TEST;

fn step(gpu: &mut GpuPhysics, steps: u32) -> Vec<Body> {
    assert!(gpu.submit(1. / 120., steps).unwrap());
    gpu.wait_readback().unwrap().unwrap()
}
fn floor(material: u32) -> Vec<u32> {
    let mut cells = vec![0; CHUNK_CELLS];
    cells[..1024].fill(material);
    cells
}

#[test]
#[ignore = "requires a headless GPU adapter"]
fn remote_regions_contacts_patches_and_impulse_targets() {
    let _guard = GPU_TEST.lock().unwrap();
    let mut gpu = GpuPhysics::new_sparse().unwrap();
    let coords = [[-120, 2, 30], [120, -2, -30]];
    assert_eq!(
        gpu.set_terrain_chunks(&[(coords[0], floor(1)), (coords[1], floor(5))])
            .unwrap(),
        ((TABLE_WORDS + 2 * PAGE_WORDS) * 4) as u64
    );
    let bodies = coords.map(|c| {
        let mut b = Body::new(
            [
                c[0] as f32 * 32. + 8.5,
                c[1] as f32 * 32. + 1.55,
                c[2] as f32 * 32. + 8.5,
            ],
            3,
        );
        b.velocity[1] = -20.;
        b
    });
    gpu.set_bodies(&bodies).unwrap();
    let result = step(&mut gpu, 1);
    let events = gpu.take_terrain_contacts();
    for i in 0..2 {
        assert!(result[i].velocity[1] >= 0., "{result:?}");
        assert!(
            events.iter().any(|e| e.target
                == [
                    coords[i][0] * 32 + 8,
                    coords[i][1] * 32,
                    coords[i][2] * 32 + 8
                ]
                && e.material == [1, 5][i]),
            "{events:?}"
        );
    }
    // Only the changed page uploads. Replacing the western floor leaves the eastern floor intact.
    assert_eq!(
        gpu.update_sparse_terrain(&coords, &[(coords[0], vec![0; CHUNK_CELLS])])
            .unwrap(),
        (PAGE_WORDS * 4) as u64
    );
    gpu.set_bodies(&bodies).unwrap();
    let result = step(&mut gpu, 1);
    assert!(result[0].velocity[1] < -19.);
    assert!(result[1].velocity[1] >= 0.);
    assert!(gpu.take_terrain_contacts().iter().all(|e| e.target[0] > 0));

    let airborne = coords.map(|c| {
        Body::new(
            [
                c[0] as f32 * 32. + 8.5,
                c[1] as f32 * 32. + 8.5,
                c[2] as f32 * 32. + 8.5,
            ],
            3,
        )
    });
    gpu.set_bodies(&airborne).unwrap();
    gpu.impulse(1, [9., 0., 0.]).unwrap();
    assert!(gpu.submit(1. / 120., 1).unwrap());
    gpu.impulse(0, [-6., 0., 0.]).unwrap();
    assert!(gpu.set_terrain_chunks(&[]).is_err());
    assert!(gpu.update_sparse_terrain(&[], &[]).is_err());
    let first = gpu.wait_readback().unwrap().unwrap();
    assert_eq!(first[0].velocity[0], 0.);
    assert!((first[1].velocity[0] - 3.).abs() < 0.001);
    assert_eq!(gpu.update_sparse_terrain(&coords, &[]).unwrap(), 0);
    let second = step(&mut gpu, 1);
    assert!((second[0].velocity[0] + 2.).abs() < 0.001);
    assert!((second[1].velocity[0] - 3.).abs() < 0.001);
    assert!(gpu.update_terrain(&[]).is_err());
    assert!(gpu.update_terrain_range(0, &[]).is_err());
}

#[test]
#[ignore = "requires a headless GPU adapter"]
fn negative_seams_residency_eviction_and_atomic_validation() {
    let _guard = GPU_TEST.lock().unwrap();
    let mut gpu = GpuPhysics::new_sparse().unwrap();
    let coords = [[-2, 0, -1], [-1, 0, -1], [0, 0, -1]];
    gpu.set_terrain_chunks(&coords.map(|c| (c, vec![0; CHUNK_CELLS])))
        .unwrap();
    let bodies = [-32.6, -0.6].map(|x| {
        let mut b = Body::new([x, 8.5, -8.5], 3);
        b.velocity[0] = 6.;
        b
    });
    gpu.set_bodies(&bodies).unwrap();
    step(&mut gpu, 8);
    let crossed = step(&mut gpu, 8);
    assert!(
        crossed[0].position[0] > -32. && crossed[1].position[0] > 0.,
        "{crossed:?}"
    );
    assert!(gpu.take_terrain_contacts().is_empty());

    let boundary = || {
        let mut b = Body::new([31.45, 8.5, -8.5], 3);
        b.velocity[0] = 20.;
        b
    };
    gpu.set_bodies(&[boundary()]).unwrap();
    let blocked = step(&mut gpu, 1);
    assert!(blocked[0].position[0] <= 31.501 && blocked[0].velocity[0] <= 0.);
    assert!(
        gpu.take_terrain_contacts().is_empty(),
        "nonresident boundary must not emit terrain contacts"
    );
    let loaded = [coords[0], coords[1], coords[2], [1, 0, -1]];
    assert!(gpu.update_sparse_terrain(&loaded, &[]).is_err());
    assert!(
        gpu.update_sparse_terrain(
            &loaded,
            &[
                (loaded[3], vec![0; CHUNK_CELLS]),
                (coords[0], vec![6; CHUNK_CELLS])
            ]
        )
        .is_err()
    );
    assert_eq!(gpu.update_sparse_terrain(&coords, &[]).unwrap(), 0);
    gpu.update_sparse_terrain(&loaded, &[(loaded[3], vec![0; CHUNK_CELLS])])
        .unwrap();
    gpu.set_bodies(&[boundary()]).unwrap();
    let free = step(&mut gpu, 8);
    assert!(free[0].position[0] > 32.);
    gpu.update_sparse_terrain(&coords, &[]).unwrap();
    gpu.set_bodies(&[boundary()]).unwrap();
    assert!(step(&mut gpu, 1)[0].position[0] <= 31.501);
    assert!(gpu.take_terrain_contacts().is_empty());
}

#[test]
#[ignore = "requires a headless GPU adapter"]
fn hashed_neighbors_do_not_duplicate_pair_contacts() {
    let _guard = GPU_TEST.lock().unwrap();
    // Find a bucket visited by two distinct neighbors of the same query cell.
    let mut collision = None;
    'search: for cx in 4..30 {
        for cy in 4..30 {
            let center = [cx, cy, 4];
            let mut seen = std::collections::BTreeMap::new();
            for x in -1..=1 {
                for y in -1..=1 {
                    for z in -1..=1 {
                        let neighbor = [cx + x, cy + y, 4 + z];
                        let hash = sparse::spatial_hash(neighbor) % sparse::BROADPHASE_BUCKETS;
                        if let Some(old) = seen.insert(hash, neighbor) {
                            collision = Some((center, old, neighbor));
                            break 'search;
                        }
                    }
                }
            }
        }
    }
    let (cell, target, alias) =
        collision.expect("test fixture requires a neighboring bucket collision");
    assert_ne!(target, alias);
    let position: [f32; 3] = std::array::from_fn(|axis| {
        cell[axis] as f32 * 2.
            + match target[axis] - cell[axis] {
                -1 => 0.3,
                1 => 1.7,
                _ => 1.,
            }
    });
    let offset: [f32; 3] = std::array::from_fn(|axis| match target[axis] - cell[axis] {
        -1 => -[0.85, 0.75, 0.65][axis],
        1 => [0.85, 0.75, 0.65][axis],
        _ => 0.1,
    });
    let make_pair = |base: [f32; 3]| {
        let a = Body::new(base, 3);
        let mut b = Body::new(std::array::from_fn(|i| base[i] + offset[i]), 3);
        b.velocity = offset.map(|v| -v * 6.);
        [a, b]
    };
    let mut gpu = GpuPhysics::new_sparse().unwrap();
    // Resident air around both the hash-collision fixture and a remote copy.
    let shift = [4096., -4096., 2048.];
    let translated: [f32; 3] = std::array::from_fn(|i| position[i] + shift[i]);
    let mut coords = std::collections::BTreeSet::new();
    for p in [position, translated] {
        let c = p.map(|v| (v.floor() as i32).div_euclid(32));
        for x in -1..=1 {
            for y in -1..=1 {
                for z in -1..=1 {
                    coords.insert([c[0] + x, c[1] + y, c[2] + z]);
                }
            }
        }
    }
    gpu.set_terrain_chunks(
        &coords
            .into_iter()
            .map(|c| (c, vec![0; CHUNK_CELLS]))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let pair = make_pair(position);
    gpu.set_bodies(&pair).unwrap();
    let reference = step(&mut gpu, 1);
    let far_pair = make_pair(translated);
    gpu.set_bodies(&[pair[0], pair[1], far_pair[0], far_pair[1]])
        .unwrap();
    let simultaneous = step(&mut gpu, 1);
    for i in 0..2 {
        for axis in 0..3 {
            assert!((reference[i].velocity[axis] - simultaneous[i].velocity[axis]).abs() < 0.002);
            assert!(
                (reference[i].velocity[axis] - simultaneous[i + 2].velocity[axis]).abs() < 0.02,
                "{reference:?} {simultaneous:?}"
            );
        }
        assert!((reference[i].damage_joules() - simultaneous[i + 2].damage_joules()).abs() < 0.02);
    }
    // Exact isolated equal-mass response on the first (minimum overlap) axis.
    let axis = (0..3)
        .max_by(|&a, &b| offset[a].abs().total_cmp(&offset[b].abs()))
        .unwrap();
    assert!(
        (reference[0].velocity[axis]
            - far_pair[1].velocity[axis] * 0.55
            - if axis == 1 { -9.81 / 120. } else { 0. })
        .abs()
            < 0.005,
        "duplicate bucket visits amplify response: {reference:?}"
    );
}
