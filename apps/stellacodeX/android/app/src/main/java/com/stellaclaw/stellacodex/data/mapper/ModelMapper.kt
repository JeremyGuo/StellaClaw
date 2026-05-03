package com.stellaclaw.stellacodex.data.mapper

import com.stellaclaw.stellacodex.data.dto.ModelDto
import com.stellaclaw.stellacodex.domain.model.ModelInfo

fun ModelDto.toDomain(): ModelInfo = ModelInfo(
    alias = alias,
    modelName = modelName,
    providerType = providerType,
)
