import fs from "node:fs";

function parseHex(value) {
  if (typeof value === "number" && Number.isInteger(value)) return value;
  if (typeof value !== "string" || !/^(?:0x)?[0-9a-f]+$/i.test(value)) return null;
  return Number.parseInt(value.replace(/^0x/i, ""), 16);
}

export function isCoherentPeImage(pe) {
  if (pe?.mz !== true || pe?.pe !== true) return false;
  const machine = parseHex(pe.machine);
  const magic = parseHex(pe.optional_header_magic);
  return (machine === 0x014c && magic === 0x010b) ||
    (machine === 0x8664 && magic === 0x020b);
}

// A Tauri NSIS x64 bundle can use a 32-bit x86 bootstrapper. The separately
// verified feed/build target and installed application establish payload arch.
export function isSupportedSetupPe(pe, payloadTargetArch) {
  return payloadTargetArch === "x64" && isCoherentPeImage(pe);
}

export function isX64ApplicationPe(pe) {
  return isCoherentPeImage(pe) &&
    parseHex(pe.machine) === 0x8664 &&
    parseHex(pe.optional_header_magic) === 0x020b;
}

export function readPeInfo(filePath) {
  const fd = fs.openSync(filePath, "r");
  try {
    const fileSize = fs.fstatSync(fd).size;
    const readAt = (size, offset) => {
      if (!Number.isSafeInteger(offset) || offset < 0 || offset + size > fileSize) return null;
      const buffer = Buffer.alloc(size);
      return fs.readSync(fd, buffer, 0, size, offset) === size ? buffer : null;
    };
    const signature = readAt(2, 0);
    const offsetBuf = readAt(4, 0x3c);
    const peOffset = offsetBuf ? offsetBuf.readUInt32LE(0) : null;
    const peSignature = peOffset == null ? null : readAt(4, peOffset);
    const isPe = peSignature?.equals(Buffer.from([0x50, 0x45, 0, 0])) === true;
    const machine = isPe ? readAt(2, peOffset + 4) : null;
    const optionalMagic = isPe ? readAt(2, peOffset + 24) : null;
    const machineCode = machine ? machine.readUInt16LE(0) : null;
    const magicCode = optionalMagic ? optionalMagic.readUInt16LE(0) : null;
    return {
      mz: signature?.toString("ascii") === "MZ",
      pe: isPe,
      machine: machineCode == null ? null : `0x${machineCode.toString(16).padStart(4, "0")}`,
      optional_header_magic: magicCode == null ? null : `0x${magicCode.toString(16).padStart(4, "0")}`,
      architecture: machineCode === 0x014c ? "x86" : machineCode === 0x8664 ? "x64" : "unknown",
    };
  } finally {
    fs.closeSync(fd);
  }
}
