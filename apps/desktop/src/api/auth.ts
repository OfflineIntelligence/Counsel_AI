// Authentication API for user login/signup

import { getApiBaseSync } from './backendUrl';
export { getApiBaseSync };

export interface LoginRequest {
    email: string;
    password: string;
}

export interface SignupRequest {
    name: string;
    email: string;
    password: string;
}

export interface UserData {
    id: number;
    name: string;
    email: string;
    email_verified: boolean;
}

export interface AuthResponse {
    success: boolean;
    message: string;
    user?: UserData;
    token?: string;
    requires_verification?: boolean;
}

const TOKEN_KEY = 'offline-intelligence-auth-token';

export function getStoredToken(): string | null {
    return localStorage.getItem(TOKEN_KEY);
}

export function setStoredToken(token: string | null): void {
    if (token) {
        localStorage.setItem(TOKEN_KEY, token);
    } else {
        localStorage.removeItem(TOKEN_KEY);
    }
}

export function clearStoredAuth(): void {
    localStorage.removeItem(TOKEN_KEY);
}

export async function getCurrentUser(token: string): Promise<AuthResponse | null> {
    try {
        const response = await fetch(`${getApiBaseSync()}/auth/me`, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
            },
            body: JSON.stringify({ token }),
        });

        if (response.ok) {
            return await response.json();
        }
        return null;
    } catch (error) {
        console.error('Failed to get current user:', error);
        return null;
    }
}

export async function loginUser(email: string, password: string): Promise<AuthResponse> {
    try {
        const response = await fetch(`${getApiBaseSync()}/auth/login`, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
            },
            body: JSON.stringify({ email, password }),
        });

        const data = await response.json();
        
        if (response.ok && data.success) {
            if (data.token) {
                setStoredToken(data.token);
            }
            return data;
        } else {
            return {
                success: false,
                message: data.message || 'Login failed',
                requires_verification: data.requires_verification || false,
            };
        }
    } catch (error) {
        console.error('Login error:', error);
        return {
            success: false,
            message: 'Unable to connect to server. Please make sure the backend is running.',
        };
    }
}

export async function signupUser(name: string, email: string, password: string): Promise<AuthResponse> {
    try {
        const response = await fetch(`${getApiBaseSync()}/auth/signup`, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
            },
            body: JSON.stringify({ name, email, password }),
        });

        const data = await response.json();
        
        if (response.ok && data.success) {
            return data;
        } else {
            return {
                success: false,
                message: data.message || 'Signup failed',
            };
        }
    } catch (error) {
        console.error('Signup error:', error);
        return {
            success: false,
            message: 'Unable to connect to server. Please make sure the backend is running.',
        };
    }
}

export async function verifyEmail(token: string): Promise<AuthResponse> {
    try {
        const response = await fetch(`${getApiBaseSync()}/auth/verify-email`, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
            },
            body: JSON.stringify({ token }),
        });

        const data = await response.json();
        
        if (response.ok && data.success) {
            if (data.token) {
                setStoredToken(data.token);
            }
            return data;
        } else {
            return {
                success: false,
                message: data.message || 'Verification failed',
            };
        }
    } catch (error) {
        console.error('Verification error:', error);
        return {
            success: false,
            message: 'Unable to connect to server.',
        };
    }
}
