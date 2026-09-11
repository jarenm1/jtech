;; Native terrain graph, server package API v1.
;;
;; The loader evaluates this file once and compiles the declared expression
;; graph and biome table into a native voxel_world::TerrainGenerator. No Scheme
;; runs during chunk generation.
;;
;; Primitive list (see crates/game_packages/src/terrain.rs for the full API):
;;   (terrain-x) (terrain-z) (constant v)
;;   (fbm freq octaves lacunarity gain salt)      value noise in [0, 1]
;;   (fbm-xy ... x z)                             samples at explicit coordinates
;;   (ridged ...) / (ridged-xy ...)               folded ridged noise in [0, 1]
;;   (tadd a b) (tsub a b) (tmul a b) (tdiv a b) (tmin a b) (tmax a b)
;;   (tabs v) (tneg v) (tsqrt v) (tpow v e)
;;   (tclamp v lo hi) (tmix a b t) (tsmoothstep e0 e1 x) (tstep edge x)
;;   (tsmooth-min a b k) (tsmooth-max a b k) (tscale-bias v scale bias)
;;   (tcurve x (list (list x0 y0) (list x1 y1) ...))
;;
;; Biomes are evaluated in declaration order against (temperature, moisture,
;; raw height) with membership fading across range edges; earlier biomes claim
;; coverage first and the last biome is the fallback. Soil depth blends across
;; the weighted contributions and a seeded hash picks the material pair. The
;; slope override replaces a surface with stone whenever the neighbouring
;; height delta reaches stone-slope blocks.

(define terrain-api-version 1)
(define terrain-version 1)
(define generator-identity "jtech-terrain-v1")
(define stone-slope 4.0)

;; Broad gentle plains, concentrated ridged mountain regions, and a connected
;; valley network carved where the ridged field's complement is high.
(define terrain-height
  (let* ((base (constant 44.0))
         (plains-relief
           (tmul (tscale-bias (fbm 0.010416667 3 2.0 0.5 102) 1.0 -0.5)
                 (constant 12.0)))
         (mask (tsmoothstep (constant 0.52) (constant 0.74)
                            (fbm 0.0013020833 3 2.0 0.55 103)))
         (ridge (tpow (ridged 0.004464286 5 2.0 0.5 104) 2.0))
         (relief (tmul mask ridge))
         (mountains (tmul relief (constant 340.0)))
         (valley (tsmoothstep (constant 0.42) (constant 0.78)
                              (tsub (constant 1.0)
                                    (ridged 0.003125 4 2.0 0.5 105))))
         (carve (tmul (tmul valley (tsub (constant 1.0) relief))
                      (constant 38.0))))
    (tclamp (tsub (tadd (tadd base plains-relief) mountains) carve)
            (constant 2.0) (constant 350.0))))

;; Temperature: a broad climate field cooled by the column's actual altitude.
(define terrain-temperature
  (tclamp (tsub (fbm 0.0011111111 3 2.0 0.5 106)
                (tmul (tsmoothstep (constant 80.0) (constant 260.0)
                                   terrain-height)
                      (constant 0.5)))
          (constant 0.0) (constant 1.0)))

(define terrain-moisture
  (tclamp (fbm 0.0015625 3 2.0 0.5 107) (constant 0.0) (constant 1.0)))

;; Soil factor; the matched biome's depth scales it into blocks.
(define terrain-soil
  (tclamp (tadd (tmul (tscale-bias (fbm 0.0078125 3 2.0 0.5 108) 1.0 -0.5)
                      (constant 0.6))
                (constant 0.5))
          (constant 0.0) (constant 1.0)))

(define biomes
  (list
      (biome "shore" 'sand 'sand 3 -2.0 3.0 -2.0 3.0 -1000.0 6.0)
    (biome "desert" 'sand 'sand 5 0.62 3.0 -2.0 0.34 -1000.0 1000.0)
    (biome "mountains" 'stone 'stone 1 -2.0 3.0 -2.0 3.0 150.0 5000.0)
    (biome "tundra" 'grass 'dirt 2 -2.0 0.3 -2.0 3.0 -1000.0 5000.0)
    (biome "plains" 'grass 'dirt 4 -2.0 3.0 -2.0 3.0 -1000.0 5000.0)))
