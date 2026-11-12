hls.js 1.7.4
https://github.com/video-dev/hls.js

Copia fija, verificada contra el paquete de npm:
  tarball sha1   1857f0cf1b849da5d7ea86b42bce8a321bf912de
  hls.min.js sha256  cfc70cc9e9fc63d071919e087670abc760c8fd1bc889b88588026e4d22193bbe

Va en el repositorio en vez de cargarse de un CDN público por dos razones: el
CSP sigue siendo 'self' (ningún tercero puede ejecutar código en la página) y
el directo no se cae si ese tercero no responde.

Safari no lo necesita: reproduce HLS por sí mismo. En Chrome y Firefox sí hace
falta, porque no traen HLS.

Para actualizarlo:
  curl -s https://registry.npmjs.org/hls.js/<versión> | grep -o '"tarball":"[^"]*"'
  curl -sL -o hls.tgz <tarball>            # comprueba el sha1 que da el registro
  tar -xzf hls.tgz package/dist/hls.min.js package/LICENSE
  cp package/dist/hls.min.js ui/vendor/hls.min.js
  cp package/LICENSE ui/vendor/hls.LICENSE
Después anota aquí la versión y los checksums nuevos.
