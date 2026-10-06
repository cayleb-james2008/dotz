import assert from "node:assert/strict";
import { isSupportedSetupPe, isX64ApplicationPe, readPeInfo } from "./windows-pe-validation.mjs";

const pe32NsisStub = { mz: true, pe: true, machine: "0x014c", optional_header_magic: "0x010b" };
const pe32PlusX64 = { mz: true, pe: true, machine: "0x8664", optional_header_magic: "0x020b" };

assert.equal(isSupportedSetupPe(pe32NsisStub, "x64"), true, "an i386 NSIS wrapper may carry a verified x64 payload");
assert.equal(isSupportedSetupPe(pe32PlusX64, "x64"), true, "an AMD64 setup PE is also a valid x64-package wrapper");
assert.equal(isSupportedSetupPe(pe32NsisStub, "x86"), false, "x86 payload packages are outside this acceptance target");
assert.equal(isSupportedSetupPe({ ...pe32NsisStub, machine: "0x8664" }, "x64"), false, "PE32 machine/magic mismatch is rejected");
assert.equal(isSupportedSetupPe({ ...pe32PlusX64, optional_header_magic: "0x010b" }, "x64"), false, "PE32+ machine/magic mismatch is rejected");
assert.equal(isSupportedSetupPe({ ...pe32NsisStub, pe: false }, "x64"), false, "non-PE wrappers are rejected");
assert.equal(isX64ApplicationPe(pe32PlusX64), true, "installed x64 app must be AMD64 PE32+");
assert.equal(isX64ApplicationPe(pe32NsisStub), false, "an i386 wrapper is not proof of an x64 installed app");
assert.equal(isX64ApplicationPe({ ...pe32PlusX64, machine: "0x014c" }), false, "an x86 payload must not pass the installed-app gate");

if (process.argv[2]) {
  const actual = readPeInfo(process.argv[2]);
  assert.equal(actual.mz, true, "the actual setup asset has an MZ header");
  assert.equal(actual.pe, true, "the actual setup asset has a PE header");
  assert.equal(isSupportedSetupPe(actual, "x64"), true, "the real i386 NSIS wrapper is accepted for the pinned x64 payload");
  console.log(`actual setup PE parser checks passed (3 assertions): ${JSON.stringify(actual)}`);
}

console.log("synthetic Windows installer PE classification tests passed (9 assertions)");
