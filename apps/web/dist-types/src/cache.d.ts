export declare function cacheEncrypted(projectId: string, value: unknown, secret: string): Promise<void>;
export declare function readEncrypted<T>(projectId: string, secret: string): Promise<T | undefined>;
