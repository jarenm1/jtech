// Return the unpacked cell address, or END for nonresident terrain.
// Sparse metadata and packed pages share one storage binding.
fn terrain_cell_index(cell: vec3<i32>) -> u32 {
    if params.terrain_mode==0u {
        let p=cell-params.origin;
        if any(p<vec3(0)) || any(p>=vec3<i32>(params.size)) { return END; }
        let q=vec3<u32>(p);
        return q.x+params.size.x*(q.z+params.size.z*q.y);
    }
    // Arithmetic shift is floor division, including negative chunk seams.
    let chunk=cell >> vec3(5u);
    let local=vec3<u32>(cell & vec3(31));
    var bucket=spatial_hash(chunk) % TERRAIN_TABLE_CAPACITY;
    for (var probe=0u; probe<TERRAIN_TABLE_CAPACITY; probe++) {
        let base=bucket*4u;
        let page=terrain[base+3u];
        if page==0u { return END; }
        let key=vec3<i32>(bitcast<i32>(terrain[base]),bitcast<i32>(terrain[base+1u]),bitcast<i32>(terrain[base+2u]));
        if all(key==chunk) {
            return (page-1u)*32768u+local.x+32u*(local.z+32u*local.y);
        }
        bucket=(bucket+1u) % TERRAIN_TABLE_CAPACITY;
    }
    return END;
}
fn terrain_material(cell: vec3<i32>) -> u32 {
    let index=terrain_cell_index(cell);
    // Nonresident cells contain bodies but cannot produce editable terrain events.
    if index==END { return 3u; }
    if params.terrain_mode==0u { return terrain[index]; }
    let word=terrain[TERRAIN_TABLE_CAPACITY*4u+index/8u];
    return (word >> ((index%8u)*4u)) & 15u;
}
fn solid(cell: vec3<i32>) -> bool { return terrain_material(cell)!=0u; }
