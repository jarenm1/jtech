;; Base melee weapons, server package API v1.
;; Edit and save this file to reload; new swings use the new generation.
;;
;; Each (melee-weapon id name range damage cooldown-ticks knockback) registers
;; one item id in the shared melee table. Ids must exceed the bow's id 6 and
;; must not collide with ids other loaded packages claim. range is metres,
;; damage whole health points, cooldown-ticks fixed60 ticks, knockback kg·m/s.
;; Head-zone hits still double damage; hands stay the unarmed fallback.
;; Weapons are equipment: each copy takes an inventory slot and drops on death.
(define package-api-version 1)
(define package-kind "melee")

(define weapons
  (list
    ;; Fast, weak, short reach; ships a 3D model under assets/.
    (melee-weapon 7 "Knife" 2.5 8 18 200.0 "knife.glb")
    ;; Slow, heavy, crushing.
    (melee-weapon 8 "War Hammer" 3.0 30 60 900.0)
    ;; Long reach, moderate everything.
    (melee-weapon 9 "Spear" 5.0 14 36 400.0)))

;; Items every player receives on connect and respawn: (id count) pairs that
;; must reference weapons this package registers.
(define spawn-items
  (list (list 7 1) (list 8 1) (list 9 1)))
