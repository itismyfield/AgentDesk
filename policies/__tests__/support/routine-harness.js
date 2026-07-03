const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const REPO_ROOT = path.resolve(__dirname, "..", "..", "..");
const FIXTURE_ROOT = path.resolve(__dirname, "..", "fixtures");

function resolveRoutinePath(relativePath) {
  if (process.env.AGENTDESK_TEST_ROUTINES_SRC && relativePath.startsWith("routines/")) {
    return path.join(
      process.env.AGENTDESK_TEST_ROUTINES_SRC,
      relativePath.slice("routines/".length)
    );
  }

  const fixturePath = path.join(FIXTURE_ROOT, relativePath);
  if (fs.existsSync(fixturePath)) {
    return fixturePath;
  }

  return path.join(REPO_ROOT, relativePath);
}

function loadRoutine(relativePath) {
  const absPath = resolveRoutinePath(relativePath);
  const source = fs.readFileSync(absPath, "utf8");

  let registeredRoutine = null;
  const agentdesk = {
    routines: {
      register(def) {
        registeredRoutine = def;
      },
    },
  };

  const context = vm.createContext({
    agentdesk,
    console,
    Date,
    JSON,
    Math,
    Object,
    Array,
    String,
    Number,
    Boolean,
    RegExp,
    Error,
    Set,
    Map,
    parseInt,
    parseFloat,
    isNaN,
    isFinite,
    undefined,
  });

  vm.runInContext(source, context, { filename: absPath });

  if (!registeredRoutine) {
    throw new Error(`No routine registered by ${relativePath}`);
  }

  return {
    routine: registeredRoutine,
    tick: (ctx) => registeredRoutine.tick(ctx),
  };
}

module.exports = { loadRoutine };
