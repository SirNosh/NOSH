declare module "qrcode-terminal" {
  const qr: { generate(value: string, options: { small: boolean }, callback: (image: string) => void): void };
  export default qr;
}
