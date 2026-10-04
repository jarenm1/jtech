;; Explosive bow, server package API v1.
;; Edit and save this file to reload. Existing arrows retain their firing generation.
(define package-api-version 1)
(define package-kind "bow")
(define shots-per-second 25)

;; Power is 0 (low), 1 (standard), 2 (high), or 3 (extreme).
(define (power-scale power)
  (list-ref '(0.5 1.0 2.0 4.0) power))

(define (projectile-for-power power)
  ;; speed m/s, gravity m/s², maximum travel m, lifetime fixed60 ticks
  (projectile 36.0 3.0 100000.0 18000))

(define (blast-for-power power)
  (let ((scale (power-scale power)))
    ;; radius m, material energy J, player launch speed m/s,
    ;; absorbed fraction, pressure pulse duration s
    (explosion (list-ref '(3.0 4.0 5.0 6.0) power)
               (* 6000.0 scale)
               (* 18.0 (sqrt scale))
               0.35
               0.00075)))

;; (sound event path) pairs; the client plays them on the matching event.
;; Paths are relative to this package's assets/ dir; a missing file is silent.
(define sounds
  (list (list "fire" "bow-fire.ogg")
        (list "hit" "explosion.ogg")))
