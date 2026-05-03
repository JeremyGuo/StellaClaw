package com.stellaclaw.stellacodex.data.dto

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
data class ModelsResponseDto(
    @SerialName("default_model") val defaultModel: String? = null,
    val total: Int = 0,
    val models: List<ModelDto> = emptyList(),
)

@Serializable
data class ModelDto(
    val alias: String = "",
    @SerialName("model_name") val modelName: String = "",
    @SerialName("provider_type") val providerType: String = "",
)
