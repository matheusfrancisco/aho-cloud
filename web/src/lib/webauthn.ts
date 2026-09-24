// WebAuthn glue: the control plane sends `{publicKey: {...}}` options with base64url binary
// fields; the browser API wants ArrayBuffers; the server wants the resulting credential back as
// JSON with base64url binary fields again.
import type { ApprovalProof, Session } from "../gen/api";
import type { Api } from "./api";
import { approvalChallenge } from "./approval";
import { decode, encode } from "./base64url";
import { signIn } from "./session";

type Obj = Record<string, unknown>;

function obj(v: unknown, what: string): Obj {
  if (typeof v !== "object" || v === null || Array.isArray(v)) throw new Error(`passkey options: ${what} is not an object`);
  return v as Obj;
}

function b64field(o: Obj, key: string, what: string): Uint8Array<ArrayBuffer> {
  const v = o[key];
  if (typeof v !== "string") throw new Error(`passkey options: ${what}.${key} is missing`);
  return decode(v);
}

/** Accepts the server's `{publicKey: {...}}` (or a bare options object). */
function publicKey(options: unknown): Obj {
  const o = obj(options, "options");
  return o.publicKey === undefined ? o : obj(o.publicKey, "publicKey");
}

function descriptors(list: unknown): PublicKeyCredentialDescriptor[] | undefined {
  if (list === undefined || list === null) return undefined;
  if (!Array.isArray(list)) throw new Error("passkey options: credential list is not an array");
  return list.map((d, i) => {
    const o = obj(d, `credential ${i}`);
    return { ...o, type: "public-key", id: b64field(o, "id", `credential ${i}`) } as unknown as PublicKeyCredentialDescriptor;
  });
}

/** Server creation options → `navigator.credentials.create({ publicKey })` input. */
export function creationOptions(options: unknown): PublicKeyCredentialCreationOptions {
  const pk = publicKey(options);
  const user = obj(pk.user, "user");
  return {
    ...pk,
    challenge: b64field(pk, "challenge", "publicKey"),
    user: { ...user, id: b64field(user, "id", "user") },
    excludeCredentials: descriptors(pk.excludeCredentials),
  } as unknown as PublicKeyCredentialCreationOptions;
}

/** Server request options → `navigator.credentials.get({ publicKey })` input; `challenge` overrides the server's. */
export function requestOptions(options: unknown, challenge?: Uint8Array<ArrayBuffer>): PublicKeyCredentialRequestOptions {
  const pk = publicKey(options);
  return {
    ...pk,
    challenge: challenge ?? b64field(pk, "challenge", "publicKey"),
    allowCredentials: descriptors(pk.allowCredentials),
  } as unknown as PublicKeyCredentialRequestOptions;
}

/** The fields of a `PublicKeyCredential` the control plane reads. */
export interface CredentialLike {
  id: string;
  rawId: ArrayBuffer;
  type: string;
  response: {
    clientDataJSON: ArrayBuffer;
    attestationObject?: ArrayBuffer;
    authenticatorData?: ArrayBuffer;
    signature?: ArrayBuffer;
    userHandle?: ArrayBuffer | null;
  };
}

/** A created or asserted credential as JSON with base64url binary fields. */
export function credentialJson(cred: CredentialLike): Obj {
  const r = cred.response;
  const response: Obj = { clientDataJSON: encode(r.clientDataJSON) };
  if (r.attestationObject !== undefined) {
    response.attestationObject = encode(r.attestationObject);
  } else {
    if (r.authenticatorData === undefined || r.signature === undefined) throw new Error("passkey: the authenticator returned neither an attestation nor an assertion");
    response.authenticatorData = encode(r.authenticatorData);
    response.signature = encode(r.signature);
    response.userHandle = r.userHandle === undefined || r.userHandle === null ? null : encode(r.userHandle);
  }
  return { id: cred.id, rawId: encode(cred.rawId), type: cred.type, response };
}

function credentials(): CredentialsContainer {
  if (typeof PublicKeyCredential === "undefined" || navigator.credentials === undefined) {
    throw new Error("This browser does not support passkeys (WebAuthn).");
  }
  return navigator.credentials;
}

async function create(options: unknown): Promise<Obj> {
  const cred = await credentials().create({ publicKey: creationOptions(options) });
  if (cred === null) throw new Error("passkey registration was cancelled");
  return credentialJson(cred as unknown as CredentialLike);
}

async function get(options: unknown, challenge?: Uint8Array<ArrayBuffer>): Promise<Obj> {
  const cred = await credentials().get({ publicKey: requestOptions(options, challenge) });
  if (cred === null) throw new Error("passkey sign-in was cancelled");
  return credentialJson(cred as unknown as CredentialLike);
}

/** Registers a new passkey (or adds one to the signed-in user) and signs in. */
export async function registerPasskey(api: Api, email: string, name: string): Promise<Session> {
  const start = await api.passkeyRegisterStart({ email, name });
  const session = await api.passkeyRegisterFinish({ ceremony_id: start.ceremony_id, credential: await create(start.options) });
  signIn(session);
  return session;
}

export async function loginPasskey(api: Api, email: string): Promise<Session> {
  const start = await api.passkeyLoginStart({ email });
  const session = await api.passkeyLoginFinish({ ceremony_id: start.ceremony_id, credential: await get(start.options) });
  signIn(session);
  return session;
}

/**
 * Signs `message` with the user's passkey: an assertion whose challenge is SHA-256(message).
 * The login-start options supply the RP id and the user's credential ids; their challenge is replaced.
 */
export async function passkeyProof(api: Api, email: string, message: string): Promise<ApprovalProof> {
  const start = await api.passkeyLoginStart({ email });
  return { method: "passkey", credential: await get(start.options, approvalChallenge(message)) };
}
