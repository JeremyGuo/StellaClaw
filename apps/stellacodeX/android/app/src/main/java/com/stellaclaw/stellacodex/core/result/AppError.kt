package com.stellaclaw.stellacodex.core.result

sealed interface AppError {
    data object MissingConnection : AppError
    data class Network(val message: String) : AppError
    data class Unauthorized(val message: String = "Unauthorized") : AppError
    data class Server(val code: Int, val message: String) : AppError
    data class Decode(val message: String) : AppError
    data class Unknown(val message: String) : AppError
}

fun AppError.userMessage(): String = when (this) {
    AppError.MissingConnection -> "Missing server URL or token."
    is AppError.Network -> "Network error: $message"
    is AppError.Unauthorized -> "Unauthorized. Check the bearer token."
    is AppError.Server -> "Server error $code: $message"
    is AppError.Decode -> "Protocol decode error: $message"
    is AppError.Unknown -> message
}
