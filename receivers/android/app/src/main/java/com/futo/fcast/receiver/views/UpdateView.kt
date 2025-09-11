package com.futo.fcast.receiver.views

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import com.futo.fcast.receiver.BuildConfig
import com.futo.fcast.receiver.R
import com.futo.fcast.receiver.composables.ThemedText
import com.futo.fcast.receiver.composables.colorButtonPrimary
import com.futo.fcast.receiver.composables.colorButtonSecondary
import com.futo.fcast.receiver.models.MainActivityViewModel

@Composable
fun ProgressBarView(viewModel: MainActivityViewModel) {
    val animatedProgress by animateFloatAsState(targetValue = viewModel.updateProgress)

    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(40.dp)
            .padding(top = 10.dp)
            .clip(RoundedCornerShape(50.dp))
            .border(1.dp, Color(0xFF4E4E4E), RoundedCornerShape(50.dp))
            .background(
                Brush.verticalGradient(
                    colors = listOf(Color(0x80141414), Color(0x80505050))
                )
            )
    ) {
        Box(
            modifier = Modifier
                .fillMaxHeight()
                .fillMaxWidth(animatedProgress)
                .clip(RoundedCornerShape(50.dp))
                .background(
                    Brush.verticalGradient(
                        colors = listOf(Color(0xFF008BD7), Color(0xFF0069AA))
                    )
                )
        )
    }
}

@Composable
fun UpdateView(viewModel: MainActivityViewModel, modifier: Modifier = Modifier) {
    if (viewModel.updateAvailable) {
        @Suppress("SENSELESS_COMPARISON")
        if (!BuildConfig.IS_PLAYSTORE_VERSION) {
            Column(
                modifier = modifier,
                horizontalAlignment = Alignment.CenterHorizontally
            ) {
                ThemedText(stringResource(R.string.update_available), Modifier.padding(top = 10.dp), FontWeight.Bold)
                Spacer(
                    modifier = Modifier
                        .padding(vertical = 10.dp)
                        .height(1.dp)
                        .fillMaxWidth()
                        .background(Color.Gray)
                )

                Row(
                    modifier = Modifier.padding(bottom = 10.dp),
                    verticalAlignment = Alignment.CenterVertically
                ) {
                    if (viewModel.updateStatus != null) {
                        ThemedText(viewModel.updateStatus!!)
//                        Text(
//                            text = viewModel.updateStatus!!,
//                            modifier = Modifier
//                                .padding(horizontal = 30.dp, vertical = 10.dp),
//                            color = Color.White,
//                            fontSize = 14.sp,
//                            fontFamily = interFontFamily,
//                            fontWeight = FontWeight.Normal,
//                            textAlign = TextAlign.Center,
//                            maxLines = 2,
//                            overflow = TextOverflow.Ellipsis
//                        )
                        if (viewModel.updateResultSuccessful != null) {
                            Image(
                                painter = if (viewModel.updateResultSuccessful!!)
                                    painterResource(R.drawable.checked)
                                else painterResource(R.drawable.error),
                                contentDescription = null,
                                modifier = Modifier
                                    .size(30.dp)
                            )
                        }
                    }
                }

                if (!viewModel.updating) {
                    Row(
                        modifier = Modifier,
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        Button(
                            onClick = viewModel::update,
                            modifier = Modifier,
                            enabled = true,
                            colors = ButtonDefaults.buttonColors(
                                containerColor = colorButtonPrimary
                            )
                        ) {
                            ThemedText(stringResource(R.string.update))
                        }
                        Spacer(modifier = Modifier.width(10.dp))
                        Button(
                            onClick = { viewModel.updateAvailable = false },
                            modifier = Modifier,
                            enabled = true,
                            colors = ButtonDefaults.buttonColors(
                                containerColor = colorButtonSecondary
                            )
                        ) {
                            ThemedText(stringResource(R.string.update_later))
                        }
                    }
                }
                else {
                    Row(
                        horizontalArrangement = Arrangement.Center,
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        ProgressBarView(viewModel)
                    }
                }
            }
        }
    }
}

@Preview
@Composable
fun UpdateViewPreview() {
    val viewModel = MainActivityViewModel()
    viewModel.updateAvailable = true
    viewModel.updating = false
    viewModel.updateStatus = stringResource(R.string.update_status)
    UpdateView(viewModel)
}

@Preview
@Composable
fun UpdateViewPreviewUpdating() {
    val viewModel = MainActivityViewModel()
    viewModel.updateAvailable = true
    viewModel.updating = true
    viewModel.updateStatus = stringResource(R.string.downloading_update)
    viewModel.updateProgress = 0.5f
    UpdateView(viewModel)
}

@Preview
@Composable
fun UpdateViewPreviewUpdateSuccess() {
    val viewModel = MainActivityViewModel()
    viewModel.updateAvailable = false
    viewModel.updating = false
    viewModel.updateStatus = stringResource(R.string.success)
    viewModel.updateResultSuccessful = true
    UpdateView(viewModel)
}

@Preview
@Composable
fun UpdateViewPreviewUpdateFail() {
    val viewModel = MainActivityViewModel()
    viewModel.updateAvailable = false
    viewModel.updating = false
    viewModel.updateStatus = stringResource(R.string.failed_to_update_with_error)
    viewModel.updateResultSuccessful = false
    UpdateView(viewModel)
}

