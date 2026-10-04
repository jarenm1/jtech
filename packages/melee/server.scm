;; Base weapons, server package API v1.
;; Edit and save this file to reload; new swings use the new generation.
;;
;; (melee-weapon id name range damage cooldown-ticks knockback [model]) registers
;; a melee weapon: its basic attack is a raycast swing.
;; (ranged-weapon id name range damage cooldown-ticks knockback speed gravity
;; travel lifetime [model]) registers one whose basic attack fires a projectile.
;; Ids must exceed the bow's id 6 and must not collide with ids other loaded
;; packages claim. range is metres, damage whole health points, cooldown-ticks
;; fixed60 ticks, knockback kg·m/s, speed m/s, gravity m/s², travel metres,
;; lifetime fixed60 ticks.
;; Head-zone hits still double damage; hands stay the unarmed fallback.
;; Weapons are equipment: each copy takes an inventory slot and drops on death.
(define package-api-version 1)
(define package-kind "melee")

(define weapons
  (list
    ;; A plain sword: the melee basic attack. Ships the package's 3D model.
    (melee-weapon 7 "Sword" 3.0 12 24 350.0 "knife.glb")
    ;; A plain bow: the ranged basic attack, no explosion.
    (ranged-weapon 8 "Bow" 16.0 14 30 0.0 40.0 9.0 100000.0 18000)))

;; Items every player receives on connect and respawn: (id count) pairs that
;; must reference weapons this package registers.
(define spawn-items
  (list (list 7 1) (list 8 1)))

;; (sound event path) pairs; the client plays them on the matching event.
;; Events: "draw" (a bow draw starts), "fire" (a shot leaves), "hit" (a strike).
;; Paths are relative to this package's assets/ dir; a missing file is silent.
(define sounds
  (list (list "draw" "bow-draw.ogg")
        (list "fire" "bow-fire.ogg")
        (list "hit" "arrow-hit.ogg")))
