// Server → client
export const MSG_SCAN_START = 1;
export const MSG_LAYOUT = 2;

// Client → server
export const MSG_VIEWPORT = 1;
export const MSG_NAVIGATE = 2;
export const MSG_REVEAL_DIR = 3;
export const MSG_REVEAL_FILE = 4;
export const MSG_RESCAN = 5;
export const MSG_SET_DEPTH = 6;
export const MSG_COLOR_MODE = 7;
export const MSG_FILTER_EXT = 8;
export const MSG_FILTER_SIZE = 9;
export const MSG_FILTER_NAME = 10;
export const MSG_CLEAR_FILTER = 11;
export const textEncoder = new TextEncoder();
export const textDecoder = new TextDecoder();
