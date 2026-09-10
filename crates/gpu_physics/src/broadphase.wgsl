// World-space width-two cells. Hash collisions share links, never contacts.
fn spatial_hash(cell: vec3<i32>) -> u32 {
    let c=vec3<u32>(cell);
    return (c.x*73856093u) ^ (c.y*19349663u) ^ (c.z*83492791u);
}
fn grid_cell(position: vec3<f32>) -> vec3<i32> {
    return vec3<i32>(floor(position*0.5));
}
fn grid_index(cell: vec3<i32>) -> u32 {
    return spatial_hash(cell) % BROADPHASE_BUCKETS;
}
@compute @workgroup_size(64)
fn clear_grid(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x < arrayLength(&heads) { atomicStore(&heads[id.x], END); }
}
@compute @workgroup_size(64)
fn build_grid(@builtin(global_invocation_id) id: vec3<u32>) {
    let i=id.x;
    if i>=params.count || src[i].material==0u { return; }
    let bucket=grid_index(grid_cell(src[i].position));
    // Exactly one link per active body; no per-cell capacity or overflow.
    next_body[i]=atomicExchange(&heads[bucket],i);
}
