/// <reference types="vite/client" />

interface ImageCapture {
  grabFrame(): Promise<ImageBitmap>
}

declare var ImageCapture: {
  prototype: ImageCapture
  new(track: MediaStreamTrack): ImageCapture
}
