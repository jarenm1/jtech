;; Explosive bow, server package API v1.
;; Edit and save this file to reload. In-flight projectiles keep the generation
;; that fired them; new shots use the freshly installed policy.
(define package-api-version 1)
(define package-kind "launcher")

;; The equipment item this package claims. Item ids are one namespace across
;; every package: ids below 6 are hands and blocks, and a loaded package wins
;; a contested id in sorted order.
(define launcher-item 6)
(define launcher-name "Explosive Bow")
(define shots-per-second 25)

;; One row per client-selectable preset: label, blast radius m, blast scale.
(define presets
  (list (list "0.5x" 3.0 0.5)
        (list "1x"   4.0 1.0)
        (list "2x"   5.0 2.0)
        (list "4x"   6.0 4.0)))

;; Flight: speed m/s, gravity m/s², maximum travel m, lifetime fixed60 ticks.
;; Blast: radius m, material energy J, player launch m/s, absorbed fraction,
;; pressure pulse s.
(define powers
  (map (lambda (preset)
         (launcher-power (car preset)
                         (projectile 36.0 3.0 64.0 180)
                         (explosion (cadr preset)
                                    (* 6000.0 (caddr preset))
                                    (* 18.0 (sqrt (caddr preset)))
                                    0.35
                                    0.00075)))
       presets))
