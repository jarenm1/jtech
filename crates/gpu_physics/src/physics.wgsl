struct Body { position: vec3<f32>, material: u32, velocity: vec3<f32>, damage_sleep: u32, fracture_damage: f32, pad0: f32, pad1: f32, pad2: f32 }
struct Params { origin: vec3<i32>, count: u32, size: vec3<u32>, dt: f32, player_count: u32, terrain_mode: u32, pad1: u32, pad2: u32 }
struct PlayerCollider { position: vec3<f32>, id: u32, velocity: vec3<f32>, padding: u32 }
@group(0) @binding(0) var<storage, read> src: array<Body>;
@group(0) @binding(1) var<storage, read_write> dst: array<Body>;
@group(0) @binding(2) var<storage, read> terrain: array<u32>;
@group(0) @binding(3) var<storage, read_write> impulses: array<vec4<f32>>;
@group(0) @binding(4) var<uniform> params: Params;
@group(0) @binding(5) var<storage, read_write> heads: array<atomic<u32>>;
@group(0) @binding(6) var<storage, read_write> next_body: array<u32>;
@group(0) @binding(7) var<storage, read> players: array<PlayerCollider>;
struct TerrainContact { cell: vec3<i32>, material: u32, dissipated_energy: f32, force: f32, area: f32, padding: f32 }
struct TerrainEvents { count: atomic<u32>, pad0: u32, pad1: u32, pad2: u32, contacts: array<TerrainContact> }
@group(0) @binding(8) var<storage, read_write> events: TerrainEvents;
const END = 0xffffffffu;
fn overlaps(p: vec3<f32>) -> bool {
    let lo = vec3<i32>(floor(p-vec3(0.4999)));
    let hi = vec3<i32>(floor(p+vec3(0.4999)));
    for (var y=lo.y; y<=hi.y; y++) { for (var z=lo.z; z<=hi.z; z++) { for (var x=lo.x; x<=hi.x; x++) {
        if solid(vec3(x,y,z)) { return true; }
    } } }
    return false;
}
fn damage_energy(m: Material, energy: f32, force: f32, area: f32) -> f32 {
    if area<=0. || force<=0. || energy<=0. { return 0.; }
    let t=clamp(force/area/m.damage_onset-1.,0.,1.);
    return max(0.,energy)*m.fracture_efficiency*t*t*(3.-2.*t);
}
fn with_damage(body: Body, damage: f32) -> Body {
    var b=body;
    b.fracture_damage+=damage;
    b.damage_sleep=u32(min(65535.,b.fracture_damage)) | (b.damage_sleep & 0xffff0000u);
    return b;
}
fn contact_area(p: vec3<f32>, cell: vec3<i32>, axis: u32) -> f32 {
    var area=1.;
    for (var k=0u; k<3u; k++) {
        if k!=axis { area*=max(0.,min(p[k]+0.5,f32(cell[k])+1.)-max(p[k]-0.5,f32(cell[k]))); }
    }
    return area;
}
fn emit_terrain(cell: vec3<i32>, mat: u32, energy: f32, force: f32, area: f32) {
    if terrain_cell_index(cell)==END { return; }
    let m=MATERIALS[mat];
    // One unit support face is the conservative attachment limit. CPU revalidates support.
    if area<=0. || (force/area<=m.damage_onset && force<=m.attachment_strength) { return; }
    let slot=atomicAdd(&events.count,1u);
    if slot<arrayLength(&events.contacts) {
        events.contacts[slot]=TerrainContact(cell,mat,energy,force,area,0.);
    }
}
// The transverse face overlaps at most four terrain voxels. Divide total impulse
// and dissipated work by actual overlap, then assign half the work to each material.
// Returns (body damage, restitution); the caller applies the matching rebound.
fn terrain_impact(p: vec3<f32>, axis: u32, incoming: f32, mat: u32) -> vec2<f32> {
    let m=MATERIALS[mat];
    var lo=vec3<i32>(floor(p-vec3(0.4999)));
    var hi=vec3<i32>(floor(p+vec3(0.4999)));
    lo[axis]=i32(floor(p[axis]+sign(incoming)*0.5)); hi[axis]=lo[axis];
    var total_area=0.; var restitution=m.restitution;
    for (var y=lo.y; y<=hi.y; y++) { for (var z=lo.z; z<=hi.z; z++) { for (var x=lo.x; x<=hi.x; x++) {
        let cell=vec3(x,y,z); let terrain_mat=terrain_material(cell);
        if terrain_mat!=0u {
            total_area+=contact_area(p,cell,axis);
            restitution=min(restitution,MATERIALS[terrain_mat].restitution);
        }
    } } }
    if abs(incoming)<2. { restitution=0.; }
    if total_area<=0. { return vec2(0.,restitution); }
    let force=m.density*abs(incoming)*(1.+restitution)/CONTACT_DT;
    let energy=0.5*m.density*incoming*incoming*(1.-restitution*restitution);
    for (var y=lo.y; y<=hi.y; y++) { for (var z=lo.z; z<=hi.z; z++) { for (var x=lo.x; x<=hi.x; x++) {
        let cell=vec3(x,y,z); let terrain_mat=terrain_material(cell);
        let area=contact_area(p,cell,axis);
        if terrain_mat!=0u && area>0. {
            let fraction=area/total_area;
            emit_terrain(cell,terrain_mat,0.5*energy*fraction,force*fraction,area);
        }
    } } }
    return vec2(damage_energy(m,0.5*energy,force,total_area),restitution);
}
// Serial bounded motor pass: one total impulse budget per player per substep.
// Slot-order allocation is intentional; no atomic float races or per-iteration reset.
@compute @workgroup_size(64)
fn player_motor(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x!=0u { return; }
    for (var j=0u; j<params.player_count; j++) {
        let player=players[j]; var budget=PLAYER_PUSH_FORCE*params.dt;
        for (var i=0u; i<params.count; i++) {
            let b=src[i]; if b.material==0u || budget<=0. { continue; }
            let delta=b.position-(player.position+vec3(0.,0.9,0.));
            let depth=vec3(0.8,1.4,0.8)-abs(delta);
            if any(depth<vec3(-0.02)) { continue; }
            var axis=0u; if depth.z<depth.x { axis=2u; }
            if depth.y<depth[axis] { continue; }
            let direction=select(-1.,1.,delta[axis]>=0.);
            let desired=max(0.,clamp(player.velocity[axis],-30.,30.)*direction);
            let m=MATERIALS[b.material].density;
            let current=(b.velocity[axis]+impulses[i][axis]/m)*direction;
            // Stopping an incoming block belongs to the kinematic collision pass.
            let amount=min(budget,max(0.,desired-max(0.,current))*m);
            impulses[i][axis]+=direction*amount;
            budget-=amount;
        }
    }
}
// Axis sweeps preserve terrain clearance even for contact projection near walls.
fn project(start: vec3<f32>, correction: vec3<f32>) -> vec3<f32> {
    var p=start;
    for (var axis=0u; axis<3u; axis++) {
        var endpoint=p; endpoint[axis]+=clamp(correction[axis],-0.25,0.25);
        if overlaps(endpoint) {
            var lo=0.; var hi=1.;
            for (var k=0u; k<10u; k++) {
                let mid=(lo+hi)*0.5;
                if overlaps(mix(p,endpoint,mid)) { hi=mid; } else { lo=mid; }
            }
            p=mix(p,endpoint,lo);
        } else { p=endpoint; }
    }
    return p;
}
fn terrain_support(p: vec3<f32>) -> bool {
    return overlaps(p-vec3(0.,0.006,0.));
}
@compute @workgroup_size(64)
fn integrate(@builtin(global_invocation_id) id: vec3<u32>) {
    let i=id.x;
    if i>=params.count { return; }
    var b=src[i];
    let impulse=impulses[i].xyz;
    impulses[i]=vec4(0.);
    if b.material==0u { dst[i]=b; return; }
    let m=MATERIALS[b.material];
    b.velocity += impulse/m.density + vec3(0.,-9.81*params.dt,0.);
    // A bounded displacement (< half a cell) keeps endpoint voxel queries conservative.
    b.velocity=clamp(b.velocity,vec3(-30.),vec3(30.));
    var grounded=false;
    for (var axis=0u; axis<3u; axis++) {
        let start=b.position;
        var endpoint=start;
        endpoint[axis]+=b.velocity[axis]*params.dt;
        if overlaps(endpoint) {
            // Binary search first contact within this short axis sweep.
            var lo=0.; var hi=1.;
            for (var k=0u; k<12u; k++) {
                let mid=(lo+hi)*0.5;
                if overlaps(mix(start,endpoint,mid)) { hi=mid; } else { lo=mid; }
            }
            b.position=mix(start,endpoint,lo);
            let incoming=b.velocity[axis];
            var probe=b.position; probe[axis]+=sign(incoming)*0.002;
            let impact=terrain_impact(probe,axis,incoming,b.material);
            b=with_damage(b,impact.x);
            if axis==1u && incoming<0. { grounded=true; }
            b.velocity[axis]=-incoming*impact.y;
        } else { b.position=endpoint; }
    }
    if grounded { b.velocity.x *= max(0.,1.-m.friction*params.dt); b.velocity.z *= max(0.,1.-m.friction*params.dt); }
    // Settling is counted once, after all contact iterations.
    dst[i]=b;
}
@compute @workgroup_size(64)
fn pair_contacts(@builtin(global_invocation_id) id: vec3<u32>) {
    let i=id.x;
    if i>=params.count { return; }
    var b=src[i];
    if b.material==0u { dst[i]=b; return; }
    let m=MATERIALS[b.material];
    var correction=vec3(0.); var dv=vec3(0.); var energy=0.;
    var modeled_work=0.;
    let cell=grid_cell(b.position);
    for (var y=-1; y<=1; y++) { for (var z=-1; z<=1; z++) { for (var x=-1; x<=1; x++) {
      let neighbor=cell+vec3(x,y,z);
      var link=atomicLoad(&heads[grid_index(neighbor)]);
      loop {
        if link==END { break; }
        let j=link;
        link=next_body[j];
        if i==j { continue; }
        let other=src[j];
        if any(grid_cell(other.position)!=neighbor) { continue; }
        let delta=b.position-other.position;
        let depth=vec3(1.)-abs(delta);
        if any(depth < vec3(-0.002)) { continue; }
        var axis=0u;
        if depth.y<depth.x { axis=1u; }
        if depth.z<depth[axis] { axis=2u; }
        var n=vec3(0.);
        n[axis]=select(-1.,1.,delta[axis]>0. || (delta[axis]==0. && i>j));
        let om=MATERIALS[other.material];
        let weight=om.density/(m.density+om.density);
        // Under-relax simultaneous Jacobi corrections to avoid pile overshoot.
        correction+=n*max(0.,depth[axis]-0.0002)*weight*0.8;
        let closing=dot(b.velocity-other.velocity,n);
        if closing<0. {
            let restitution=select(0.,min(m.restitution,om.restitution),closing < -2.);
            let relaxation=select(0.8,1.,closing < -2.);
            let reduced_mass=m.density*om.density/(m.density+om.density);
            let impulse=-(1.+restitution)*closing*reduced_mass*relaxation;
            dv+=n*impulse/m.density;
            let tangent=(b.velocity-other.velocity)-n*closing;
            let friction=min(length(tangent),-closing*0.5)*weight*0.8;
            if length(tangent)>0.0001 { dv-=normalize(tangent)*friction; }
            var area=1.;
            for (var k=0u; k<3u; k++) { if k!=axis { area*=max(0.,depth[k]); } }
            // Actual normal work removed by this impulse, half for each participant.
            let dissipated=max(0.,-impulse*closing-impulse*impulse/(2.*reduced_mass));
            modeled_work+=0.5*dissipated;
            energy+=damage_energy(m,0.5*dissipated,impulse/CONTACT_DT,area);
        }
      }
    } } }
    b.position=project(b.position,correction);
    b.velocity=clamp(b.velocity+dv,vec3(-30.),vec3(30.));
    b.pad0=energy; b.pad1=modeled_work;
    dst[i]=b;
}

// Pairwise Jacobi work omits simultaneous-impulse cross terms. Cap its damage
// allocation by actual kinetic energy removed by the combined velocity update.
// Any unassigned loss is heat/damping rather than extra fracture work.
@compute @workgroup_size(64)
fn pair_budget(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x!=0u { return; }
    var removed=0.; var modeled=0.;
    for (var i=0u; i<params.count; i++) {
        if src[i].material==0u { continue; }
        let m=MATERIALS[src[i].material].density;
        removed+=0.5*m*(dot(src[i].velocity,src[i].velocity)-dot(dst[i].velocity,dst[i].velocity));
        modeled+=dst[i].pad1;
    }
    var scale=0.;
    if modeled>0. { scale=clamp(removed/modeled,0.,1.); }
    events.pad0=bitcast<u32>(scale);
}

@compute @workgroup_size(64)
fn contacts(@builtin(global_invocation_id) id: vec3<u32>) {
    let i=id.x;
    if i>=params.count { return; }
    var b=src[i];
    if b.material==0u { dst[i]=b; return; }
    let m=MATERIALS[b.material];
    var energy=b.pad0*bitcast<f32>(events.pad0);
    b.pad0=0.; b.pad1=0.;
    // Incoming body motion meets a kinematic player. Walking drive was already
    // budgeted before integration; player overlap alone never projects a body.
    for (var j=0u; j<params.player_count; j++) {
        let player=players[j];
        let delta=b.position-(player.position+vec3(0.,0.9,0.));
        let depth=vec3(0.8,1.4,0.8)-abs(delta);
        if any(depth<vec3(-0.002)) { continue; }
        var axis=0u;
        if depth.y<depth.x { axis=1u; }
        if depth.z<depth[axis] { axis=2u; }
        var n=vec3(0.);
        n[axis]=select(-1.,1.,delta[axis]>0. || (delta[axis]==0. && i>=player.id));
        let incoming=dot(b.velocity,n);
        if incoming<0. {
            // Correct only penetration attributable to incoming body travel. A
            // tiny velocity must not unlock an arbitrary player-driven shove.
            let travel=-incoming*params.dt;
            b.position=project(b.position,n*min(max(0.,depth[axis]+0.0002),travel));
            var area=1.;
            for (var k=0u; k<3u; k++) { if k!=axis { area*=max(0.,min(depth[k],select(0.6,1.,k==1u))); } }
            // Treat the player as the other half of an inelastic contact.
            energy+=damage_energy(m,0.25*m.density*incoming*incoming,-m.density*incoming/CONTACT_DT,area);
            b.velocity-=n*incoming;
        }
    }
    // Remove velocity directed into an adjacent terrain face after projection.
    for (var axis=0u; axis<3u; axis++) {
        var probe=b.position;
        probe[axis]+=sign(b.velocity[axis])*0.002;
        if overlaps(probe) && b.velocity[axis]!=0. {
            let impact=terrain_impact(probe,axis,b.velocity[axis],b.material);
            energy+=impact.x;
            b.velocity[axis]*=-impact.y;
        }
    }
    b=with_damage(b,energy);
    dst[i]=b;
}
@compute @workgroup_size(64)
fn finalize(@builtin(global_invocation_id) id: vec3<u32>) {
    let i=id.x;
    if i>=params.count { return; }
    var b=src[i];
    if b.material==0u { dst[i]=b; return; }
    var supported=terrain_support(b.position);
    let cell=grid_cell(b.position);
    for (var y=-1; y<=1; y++) { for (var z=-1; z<=1; z++) { for (var x=-1; x<=1; x++) {
        let neighbor=cell+vec3(x,y,z);
        var link=atomicLoad(&heads[grid_index(neighbor)]);
        loop {
            if link==END { break; }
            let j=link; link=next_body[j];
            if i==j { continue; }
            let other=src[j]; let delta=b.position-other.position;
            if any(grid_cell(other.position)!=neighbor) { continue; }
            // Require support history rooted at terrain, not just a co-falling pair.
            let rooted=terrain_support(other.position) || (other.damage_sleep>>16u)>0u;
            if rooted && delta.y>0.8 && delta.y<1.006 && abs(delta.x)<0.99 && abs(delta.z)<0.99 && abs(other.velocity.y)<0.6 {
                supported=true;
            }
        }
    } } }
    var sleep=0u;
    if supported && dot(b.velocity,b.velocity)<0.36 {
        sleep=min(65535u,(b.damage_sleep>>16u)+1u);
        let damping=max(0.,1.-MATERIALS[b.material].friction*params.dt);
        b.velocity.x*=damping; b.velocity.z*=damping;
    }
    // No rigid sleep: unsupported bodies always integrate gravity next substep.
    b.damage_sleep=(b.damage_sleep & 65535u) | (sleep<<16u);
    dst[i]=b;
}
