/** El enlace de los espectadores: la web vive en el VPS, con el certificado de <ip-con-guiones>.sslip.io. */
export function webLink(vps: string): string | null {
  const host = vps.trim().split("@").pop() ?? "";
  const web = /^\d+\.\d+\.\d+\.\d+$/.test(host) ? `${host.replace(/\./g, "-")}.sslip.io` : host;
  return web ? `https://${web}/` : null;
}
